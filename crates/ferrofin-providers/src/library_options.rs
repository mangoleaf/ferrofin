//! Static registry of Ferrofin's compiled-in metadata/image/subtitle/segment
//! providers, projected into the shapes the library-options API needs:
//!
//! - [`library_options_info`] → the [`LibraryOptionsResultDto`] the Add-Library
//!   wizard reads (per-type metadata/image fetchers plus the flat saver/reader/
//!   subtitle/lyric/media-segment lists), and
//! - [`all_metadata_plugins`] → the per-item-type [`MetadataPluginSummary`] list
//!   (`ProviderManager::get_all_metadata_plugins`).
//!
//! The registry reflects what is actually compiled into this build: the local
//! Kodi/XBMC **Nfo** reader/saver and **Local Images** provider are always
//! present; **The Open Movie Database** (OMDb) and **IntroSkipper** segments are
//! always compiled (OMDb needs a key at runtime, which the checkbox gates);
//! **TheMovieDb** and **Open Subtitles** appear only when their crate features
//! are enabled. Nothing here is a placeholder — a provider is listed iff its
//! code is in the binary.

use ferrofin_model::configuration::{
    ImageOption, LibraryOptionInfoDto, LibraryOptionsResultDto, LibraryTypeOptionsDto,
    MetadataPlugin, MetadataPluginSummary, MetadataPluginType,
};
use ferrofin_model::entities::ImageType;

/// The advertised provider names — the EXACT strings clients round-trip in
/// `TypeOptions.MetadataFetchers` / `ImageFetchers` (and the flat reader
/// lists), and therefore the strings the scanner's per-library gate matches
/// on. Matching Jellyfin's provider `Name` properties keeps a migrated
/// Jellyfin database's saved checkbox state meaningful. Never rename one:
/// renaming orphans every saved library's fetcher selection.
pub mod fetcher_names {
    /// The local Kodi/XBMC NFO reader/saver.
    pub const NFO: &str = "Nfo";
    /// TMDB metadata + images.
    pub const TMDB: &str = "TheMovieDb";
    /// OMDb (Rotten Tomatoes rating supplement).
    pub const OMDB: &str = "The Open Movie Database";
    /// TheTVDB series/episode metadata + artwork.
    pub const TVDB: &str = "TheTVDB";
    /// fanart.tv artwork supplement.
    pub const FANART: &str = "FanArt";
    /// MusicBrainz id resolution for music.
    pub const MUSICBRAINZ: &str = "MusicBrainz";
    /// TheAudioDB music metadata + artwork.
    pub const AUDIODB: &str = "TheAudioDB";
    /// Sidecar/art-dir image discovery.
    pub const LOCAL_IMAGES: &str = "Local Images";
    /// Cover art extracted from the media file itself.
    pub const EMBEDDED_IMAGES: &str = "Embedded Image Extractor";

    /// Every built-in fetcher name — the reserved set a dynamically
    /// registered (WASM) provider name must not collide with: a plugin
    /// declaring `"TheMovieDb"` would ride TMDB's checkbox/order and
    /// appear twice in the dashboard lists.
    pub const ALL: &[&str] = &[
        NFO,
        TMDB,
        OMDB,
        TVDB,
        FANART,
        MUSICBRAINZ,
        AUDIODB,
        LOCAL_IMAGES,
        EMBEDDED_IMAGES,
    ];
}

/// A capability a provider exposes (one provider may expose several).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cap {
    LocalMetadata,
    MetadataFetcher,
    MetadataSaver,
    LocalImage,
    ImageFetcher,
    Subtitle,
    Lyric,
    MediaSegment,
    /// A local (library-side) similarity provider.
    LocalSimilarity,
    /// A remote similarity provider.
    Similarity,
}

impl Cap {
    /// The wire plugin-type this capability reports as.
    fn plugin_type(self) -> MetadataPluginType {
        match self {
            Cap::LocalMetadata => MetadataPluginType::LocalMetadataProvider,
            Cap::MetadataFetcher => MetadataPluginType::MetadataFetcher,
            Cap::MetadataSaver => MetadataPluginType::MetadataSaver,
            Cap::LocalImage => MetadataPluginType::LocalImageProvider,
            Cap::ImageFetcher => MetadataPluginType::ImageFetcher,
            Cap::Subtitle => MetadataPluginType::SubtitleFetcher,
            Cap::Lyric => MetadataPluginType::LyricFetcher,
            Cap::MediaSegment => MetadataPluginType::MediaSegmentProvider,
            Cap::LocalSimilarity => MetadataPluginType::LocalSimilarityProvider,
            Cap::Similarity => MetadataPluginType::SimilarityProvider,
        }
    }
}

/// One registered provider.
struct Provider {
    name: &'static str,
    caps: &'static [Cap],
    /// Item types the provider applies to; empty means every type.
    types: &'static [&'static str],
    default_enabled: bool,
    /// Whether this provider's code is compiled into the build.
    compiled: bool,
}

impl Provider {
    fn applies_to(&self, type_name: &str) -> bool {
        self.types.is_empty() || self.types.contains(&type_name)
    }
    fn info(&self) -> LibraryOptionInfoDto {
        LibraryOptionInfoDto {
            name: Some(self.name.to_owned()),
            default_enabled: self.default_enabled,
        }
    }
}

/// The compiled-in provider registry (features resolved for this build).
fn providers() -> Vec<Provider> {
    let mut provs = local_providers();
    provs.extend(remote_providers());
    provs
}

/// The providers that read from the library's own files.
fn local_providers() -> Vec<Provider> {
    vec![Provider {
        name: "Nfo",
        caps: &[Cap::LocalMetadata, Cap::MetadataSaver, Cap::LocalImage],
        types: &[
            "Movie",
            "Series",
            "Season",
            "Episode",
            "MusicVideo",
            "BoxSet",
        ],
        default_enabled: true,
        compiled: true,
    }]
}

/// The providers that fetch from an external service.
fn remote_providers() -> Vec<Provider> {
    let mut provs = metadata_providers();
    provs.extend(similarity_providers());
    provs
}

/// The remote providers that supply metadata or artwork.
fn metadata_providers() -> Vec<Provider> {
    vec![
        Provider {
            // Always wired by the composition root (its client is created
            // unconditionally); the `tmdb` cargo feature gates nothing.
            name: "TheMovieDb",
            caps: &[Cap::MetadataFetcher, Cap::ImageFetcher],
            types: &["Movie", "Series", "Season", "Episode", "Person", "BoxSet"],
            default_enabled: true,
            compiled: true,
        },
        Provider {
            name: "The Open Movie Database",
            caps: &[Cap::MetadataFetcher, Cap::ImageFetcher],
            types: &["Movie", "Series", "Episode"],
            default_enabled: true,
            compiled: true,
        },
        Provider {
            // Optional at runtime (needs an API key/config), like OMDb —
            // the checkbox gates; absence of config just yields no hits.
            name: fetcher_names::TVDB,
            caps: &[Cap::MetadataFetcher, Cap::ImageFetcher],
            types: &["Series", "Season", "Episode"],
            default_enabled: true,
            compiled: true,
        },
        Provider {
            name: fetcher_names::FANART,
            caps: &[Cap::ImageFetcher],
            types: &["Movie", "Series"],
            default_enabled: true,
            compiled: true,
        },
        Provider {
            name: fetcher_names::MUSICBRAINZ,
            caps: &[Cap::MetadataFetcher],
            types: &["MusicArtist", "MusicAlbum", "Audio"],
            default_enabled: true,
            compiled: true,
        },
        Provider {
            name: fetcher_names::AUDIODB,
            caps: &[Cap::MetadataFetcher, Cap::ImageFetcher],
            types: &["MusicArtist", "MusicAlbum"],
            default_enabled: true,
            compiled: true,
        },
        Provider {
            name: fetcher_names::EMBEDDED_IMAGES,
            caps: &[Cap::ImageFetcher],
            types: &[
                "Movie",
                "Episode",
                "MusicVideo",
                "Video",
                "Audio",
                "AudioBook",
            ],
            default_enabled: true,
            compiled: true,
        },
        Provider {
            name: "Open Subtitles",
            caps: &[Cap::Subtitle],
            types: &["Movie", "Episode"],
            default_enabled: true,
            compiled: cfg!(feature = "opensubtitles"),
        },
        Provider {
            name: "Local Images",
            caps: &[Cap::LocalImage],
            types: &[],
            default_enabled: true,
            compiled: true,
        },
        Provider {
            // Upstream registers six identically-named local providers (one per
            // kind); Ferrofin's single weighted genre/tag/people scorer is the
            // same thing, so it is advertised once for every kind it serves.
            name: "Local Genre/Tag",
            caps: &[Cap::LocalSimilarity],
            types: &[
                "Movie",
                "Series",
                "Audio",
                "MusicAlbum",
                "MusicArtist",
                "Trailer",
            ],
            default_enabled: true,
            compiled: true,
        },
    ]
}

/// The remote providers that answer "what is similar to this".
fn similarity_providers() -> Vec<Provider> {
    vec![
        Provider {
            // Upstream registers only `TmdbMovieSimilarProvider` and
            // `TmdbSeriesSimilarProvider`, so TMDB's similarity capability
            // covers those two kinds and not the rest of its metadata types.
            name: "TheMovieDb",
            caps: &[Cap::Similarity],
            types: &["Movie", "Series"],
            default_enabled: false,
            compiled: true,
        },
        Provider {
            name: "ListenBrainz",
            caps: &[Cap::Similarity],
            types: &["MusicArtist"],
            default_enabled: false,
            compiled: true,
        },
        Provider {
            name: "IntroSkipper",
            caps: &[Cap::MediaSegment],
            types: &["Episode", "Movie"],
            default_enabled: true,
            compiled: true,
        },
    ]
}

/// The item types [`all_metadata_plugins`] enumerates (every type Ferrofin can
/// attach providers to).
const CANONICAL_TYPES: &[&str] = &[
    "Movie",
    "Series",
    "Season",
    "Episode",
    "Person",
    "MusicVideo",
    "BoxSet",
    "MusicAlbum",
    "MusicArtist",
    "Audio",
    "Book",
    "AudioBook",
    "Video",
    "Photo",
];

/// The image types a library of `type_name` can carry.
fn supported_image_types(type_name: &str) -> Vec<ImageType> {
    use ImageType::{
        Art, Backdrop, Banner, Box, BoxRear, Disc, Logo, Menu, Primary, Screenshot, Thumb,
    };
    match type_name {
        "Person" | "MusicArtist" => vec![Primary, Backdrop, Logo, Thumb],
        "MusicAlbum" | "Audio" => vec![Primary, Backdrop, Logo, Disc],
        "Photo" => vec![Primary],
        _ => vec![
            Primary, Art, Backdrop, Banner, Logo, Thumb, Disc, Box, Screenshot, Menu, BoxRear,
        ],
    }
}

/// The default per-type image download options (limits/min-widths).
fn default_image_options(type_name: &str) -> Vec<ImageOption> {
    use ImageType::{Backdrop, Logo, Primary};
    let mut options = vec![
        ImageOption {
            type_: Primary,
            limit: 1,
            min_width: 0,
        },
        ImageOption {
            type_: Backdrop,
            limit: 1,
            min_width: 1280,
        },
    ];
    // Non-music, non-photo (video-ish) libraries also fetch a logo by default.
    if !matches!(type_name, "MusicAlbum" | "MusicArtist" | "Audio" | "Photo") {
        options.push(ImageOption {
            type_: Logo,
            limit: 1,
            min_width: 0,
        });
    }
    options
}

/// Assembles the [`LibraryOptionsResultDto`] for a library whose representative
/// item types are `item_types`. `dynamic_fetchers` are runtime-registered
/// named metadata providers (WASM plugins) as (name, supported kinds) —
/// they appear in the fetcher lists exactly like compiled providers.
#[must_use]
pub fn library_options_info(
    item_types: &[String],
    dynamic_fetchers: &[(String, Vec<String>)],
) -> LibraryOptionsResultDto {
    let dynamic_info = |name: &str| LibraryOptionInfoDto {
        name: Some(name.to_owned()),
        default_enabled: true,
    };
    let provs = providers();
    let flat = |cap: Cap| -> Vec<LibraryOptionInfoDto> {
        provs
            .iter()
            .filter(|p| p.compiled && p.caps.contains(&cap))
            .map(Provider::info)
            .collect()
    };
    let type_options = item_types
        .iter()
        .map(|type_name| {
            let per_type = |cap: Cap| -> Vec<LibraryOptionInfoDto> {
                provs
                    .iter()
                    .filter(|p| p.compiled && p.caps.contains(&cap) && p.applies_to(type_name))
                    .map(Provider::info)
                    .collect()
            };
            let mut metadata_fetchers = per_type(Cap::MetadataFetcher);
            metadata_fetchers.extend(
                dynamic_fetchers
                    .iter()
                    .filter(|(_, kinds)| kinds.iter().any(|k| k == type_name))
                    .map(|(name, _)| dynamic_info(name)),
            );
            let mut image_fetchers = per_type(Cap::ImageFetcher);
            image_fetchers.extend(
                dynamic_fetchers
                    .iter()
                    .filter(|(_, kinds)| kinds.iter().any(|k| k == type_name))
                    .map(|(name, _)| dynamic_info(name)),
            );
            // C# `LibraryController`: local similarity providers are ticked by
            // default, remote ones are not.
            let mut similar_item_providers = per_type(Cap::LocalSimilarity);
            similar_item_providers.extend(per_type(Cap::Similarity).into_iter().map(|mut info| {
                info.default_enabled = false;
                info
            }));
            LibraryTypeOptionsDto {
                type_: Some(type_name.clone()),
                metadata_fetchers,
                image_fetchers,
                similar_item_providers,
                supported_image_types: supported_image_types(type_name),
                default_image_options: default_image_options(type_name),
            }
        })
        .collect();

    LibraryOptionsResultDto {
        metadata_savers: flat(Cap::MetadataSaver),
        metadata_readers: flat(Cap::LocalMetadata),
        subtitle_fetchers: flat(Cap::Subtitle),
        lyric_fetchers: flat(Cap::Lyric),
        media_segment_providers: flat(Cap::MediaSegment),
        type_options,
    }
}

/// The per-item-type metadata-plugin summaries. A type is included only when at
/// least one compiled provider applies to it.
#[must_use]
pub fn all_metadata_plugins() -> Vec<MetadataPluginSummary> {
    let provs = providers();
    CANONICAL_TYPES
        .iter()
        .filter_map(|&type_name| {
            let mut plugins: Vec<MetadataPlugin> = Vec::new();
            for provider in provs
                .iter()
                .filter(|p| p.compiled && p.applies_to(type_name))
            {
                for &cap in provider.caps {
                    plugins.push(MetadataPlugin {
                        name: Some(provider.name.to_owned()),
                        type_: cap.plugin_type(),
                    });
                }
            }
            if plugins.is_empty() {
                return None;
            }
            Some(MetadataPluginSummary {
                item_type: Some(type_name.to_owned()),
                plugins,
                supported_image_types: supported_image_types(type_name),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{all_metadata_plugins, library_options_info};
    use ferrofin_model::configuration::MetadataPluginType;

    #[test]
    fn movie_options_expose_real_fetchers_and_savers() {
        let info = library_options_info(&["Movie".to_owned()], &[]);
        // Nfo is a local reader + saver.
        assert!(
            info.metadata_readers
                .iter()
                .any(|o| o.name.as_deref() == Some("Nfo"))
        );
        assert!(
            info.metadata_savers
                .iter()
                .any(|o| o.name.as_deref() == Some("Nfo"))
        );
        // OMDb is always compiled and is a Movie metadata + image fetcher.
        let movie = info
            .type_options
            .iter()
            .find(|t| t.type_.as_deref() == Some("Movie"))
            .expect("movie block");
        assert!(
            movie
                .metadata_fetchers
                .iter()
                .any(|o| o.name.as_deref() == Some("The Open Movie Database"))
        );
        assert!(
            movie
                .image_fetchers
                .iter()
                .any(|o| o.name.as_deref() == Some("The Open Movie Database"))
        );
        // IntroSkipper is a media-segment provider, not a metadata fetcher.
        assert!(
            info.media_segment_providers
                .iter()
                .any(|o| o.name.as_deref() == Some("IntroSkipper"))
        );
        assert!(!movie.supported_image_types.is_empty());
    }

    #[test]
    fn tmdb_listed_for_series_and_opensubtitles_gated_by_feature() {
        let info = library_options_info(&["Series".to_owned(), "Episode".to_owned()], &[]);
        let series = info.type_options.first().expect("series block");
        // TheMovieDb is always wired, so it is always offered for a series.
        assert!(
            series
                .metadata_fetchers
                .iter()
                .any(|o| o.name.as_deref() == Some("TheMovieDb"))
        );
        // Open Subtitles is genuinely module-gated by its crate feature.
        let has_os = info
            .subtitle_fetchers
            .iter()
            .any(|o| o.name.as_deref() == Some("Open Subtitles"));
        assert_eq!(has_os, cfg!(feature = "opensubtitles"));
    }

    #[test]
    fn all_metadata_plugins_tag_capabilities_per_type() {
        let plugins = all_metadata_plugins();
        let movie = plugins
            .iter()
            .find(|p| p.item_type.as_deref() == Some("Movie"))
            .expect("movie summary");
        // Nfo appears as a local metadata provider and a saver.
        assert!(
            movie
                .plugins
                .iter()
                .any(|p| p.name.as_deref() == Some("Nfo")
                    && p.type_ == MetadataPluginType::LocalMetadataProvider)
        );
        assert!(
            movie
                .plugins
                .iter()
                .any(|p| p.name.as_deref() == Some("Nfo")
                    && p.type_ == MetadataPluginType::MetadataSaver)
        );
        // Photo has only a local image provider (Local Images), so it is present.
        assert!(
            plugins
                .iter()
                .any(|p| p.item_type.as_deref() == Some("Photo"))
        );
    }
}
