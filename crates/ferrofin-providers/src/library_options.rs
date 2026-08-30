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
    LibraryOptionInfoDto, LibraryOptionsResultDto, LibraryTypeOptionsDto, MetadataPlugin,
    MetadataPluginSummary, MetadataPluginType,
};
use ferrofin_model::entities::ImageType;

/// The `TypeOptions` entry a library saved for item type `kind`, if any.
fn type_entry<'a>(
    options: Option<&'a ferrofin_model::configuration::LibraryOptions>,
    kind: &str,
) -> Option<&'a ferrofin_model::configuration::TypeOptions> {
    options?.type_options.iter().find(|t| {
        t.type_
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(kind))
    })
}

/// Whether the library enables metadata fetcher `name` for item type `kind`.
///
/// Port of `BaseItemManager.IsMetadataFetcherEnabled` (v10.11.8
/// `MediaBrowser.Controller/BaseItemManager/BaseItemManager.cs`): when the
/// library saved a `TypeOptions` entry for the kind, the answer is exactly
/// `libraryTypeOptions.MetadataFetchers.Contains(name, OrdinalIgnoreCase)` —
/// so an EMPTY list disables every remote fetcher. With no entry the library
/// never customised that type and the built-in default (enabled) stands.
///
/// This is the ONE gate: C# routes both the scan and the on-demand
/// `POST /Items/{id}/Refresh` through `ProviderManager.CanRefreshMetadata`,
/// which calls it. Anything in Ferrofin that fetches remote metadata must ask
/// here too, or clearing the checkboxes stops meaning anything.
#[must_use]
pub fn metadata_fetcher_enabled(
    options: Option<&ferrofin_model::configuration::LibraryOptions>,
    kind: &str,
    name: &str,
) -> bool {
    type_entry(options, kind).is_none_or(|t| {
        t.metadata_fetchers
            .iter()
            .any(|f| f.eq_ignore_ascii_case(name))
    })
}

/// The library's configured `MetadataFetcherOrder` for item type `kind` — the
/// order `GetMetadataProvidersInternal` sorts remote providers by
/// (`ProviderManager.cs:445`). Empty when the library pins no order, in which
/// case the providers keep their registration order (upstream's
/// `GetDefaultOrder` tie-break).
#[must_use]
pub fn metadata_fetcher_order(
    options: Option<&ferrofin_model::configuration::LibraryOptions>,
    kind: &str,
) -> Vec<String> {
    type_entry(options, kind)
        .map(|t| t.metadata_fetcher_order.clone())
        .unwrap_or_default()
}

/// Whether the library enables image fetcher `name` for item type `kind`.
///
/// Port of `BaseItemManager.IsImageFetcherEnabled`, the image half of the same
/// gate (`ProviderManager.CanRefreshImages`).
#[must_use]
pub fn image_fetcher_enabled(
    options: Option<&ferrofin_model::configuration::LibraryOptions>,
    kind: &str,
    name: &str,
) -> bool {
    type_entry(options, kind).is_none_or(|t| {
        t.image_fetchers
            .iter()
            .any(|f| f.eq_ignore_ascii_case(name))
    })
}

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
    /// Cover art extracted from a VIDEO file itself
    /// (`MediaBrowser.Providers/MediaInfo/EmbeddedImageProvider.cs:69`).
    pub const EMBEDDED_IMAGES: &str = "Embedded Image Extractor";
    /// Cover art extracted from an AUDIO file itself — a separate upstream
    /// provider with its own name
    /// (`MediaBrowser.Providers/MediaInfo/AudioImageProvider.cs:51`).
    pub const AUDIO_IMAGES: &str = "Image Extractor";

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
        AUDIO_IMAGES,
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
    /// The image types this provider can supply for an item type — the port of
    /// each provider's `GetSupportedImages(item)`. `None` for providers that
    /// are not image fetchers.
    images: Option<fn(&str) -> &'static [ImageType]>,
}

/// The image types a non-image provider supplies: none.
const NO_IMAGES: Option<fn(&str) -> &'static [ImageType]> = None;

impl Provider {
    fn applies_to(&self, type_name: &str) -> bool {
        self.types.is_empty() || self.types.contains(&type_name)
    }
    /// The provider's `LibraryOptionInfoDto`, with `DefaultEnabled` resolved
    /// through the C# new-library rules rather than the registry's standing
    /// default.
    fn info_for(&self, cap: Cap, type_name: &str, is_new_library: bool) -> LibraryOptionInfoDto {
        LibraryOptionInfoDto {
            name: Some(self.name.to_owned()),
            default_enabled: default_enabled_for(self.name, cap, type_name, is_new_library)
                .unwrap_or(self.default_enabled),
        }
    }
    /// The image types this provider supplies for `type_name`.
    fn images_for(&self, type_name: &str) -> &'static [ImageType] {
        self.images.map_or(&[][..], |f| f(type_name))
    }
}

/// The `ServerConfiguration` constructor's built-in `MetadataOptions`
/// blocklist: `MusicVideo` disables "The Open Movie Database" for both metadata
/// and images, and `MusicAlbum`/`MusicArtist` disable "TheAudioDB" for metadata
/// (v10.11.8 `ServerConfiguration.cs:20-63`). Everything else is unrestricted.
///
/// Returns `None` when nothing in the array applies, leaving the registry
/// default in place.
fn default_metadata_options_blocklist(name: &str, cap: Cap, type_name: &str) -> Option<bool> {
    let eq = |a: &str| name.eq_ignore_ascii_case(a);
    let type_is = |a: &str| type_name.eq_ignore_ascii_case(a);
    let blocked = match cap {
        Cap::MetadataFetcher => {
            (type_is("MusicVideo") && eq(fetcher_names::OMDB))
                || ((type_is("MusicAlbum") || type_is("MusicArtist")) && eq(fetcher_names::AUDIODB))
        }
        Cap::ImageFetcher => type_is("MusicVideo") && eq(fetcher_names::OMDB),
        _ => false,
    };
    blocked.then_some(false)
}

/// The `DefaultEnabled` a fetcher/saver reports, ported from
/// `LibraryController.IsSaverEnabledByDefault` /
/// `IsMetadataFetcherEnabledByDefault` / `IsImageFetcherEnabledByDefault`
/// (byte-identical between v10.11.8 and master).
///
/// `None` means "the C# helper does not apply to this capability", leaving the
/// registry default in place.
///
/// The non-new-library branch of each helper consults
/// `MetadataOptions.Disabled*` — a per-server admin blocklist Ferrofin has no
/// store for — whose `metadataOptions is null || !Contains(name)` shape yields
/// `true` for an empty store, which is what the registry default already says.
fn default_enabled_for(
    name: &str,
    cap: Cap,
    type_name: &str,
    is_new_library: bool,
) -> Option<bool> {
    if !is_new_library {
        // The non-new-library branch of each C# helper reads
        // `ServerConfiguration.MetadataOptions`, which ships a small BUILT-IN
        // blocklist in its constructor (v10.11.8
        // `MediaBrowser.Model/Configuration/ServerConfiguration.cs:20-63`) —
        // not just admin edits. Those defaults are ported here; Ferrofin has no
        // admin-editable `Disabled*` store, and for a type the array does not
        // name, `metadataOptions is null || !Contains(name)` is `true`, which is
        // what the registry default already says.
        return default_metadata_options_blocklist(name, cap, type_name);
    }
    let eq = |a: &str| name.eq_ignore_ascii_case(a);
    let type_is = |types: &[&str]| types.iter().any(|t| type_name.eq_ignore_ascii_case(t));
    match cap {
        // `isNewLibrary` ⇒ no saver is pre-ticked, so a freshly added library
        // does not start writing NFO sidecars into the user's media folders.
        Cap::MetadataSaver => Some(false),
        Cap::MetadataFetcher => Some(if eq(fetcher_names::TMDB) {
            !type_is(&["Season", "Episode", "MusicVideo"])
        } else {
            eq(fetcher_names::TVDB) || eq(fetcher_names::AUDIODB) || eq(fetcher_names::MUSICBRAINZ)
        }),
        Cap::ImageFetcher => Some(if eq(fetcher_names::TMDB) {
            !type_is(&["Series", "Season", "Episode", "MusicVideo"])
        } else {
            // The allowlist's "Image Extractor" is upstream's AudioImageProvider
            // (`AUDIO_IMAGES`), NOT the video-side "Embedded Image Extractor",
            // which is deliberately absent — a new library does not pre-tick
            // frame/cover extraction for video. "Screen Grabber" is upstream's
            // `VideoImageProvider`, which Ferrofin does not register yet; the
            // name is kept so the arm stays a faithful transliteration.
            eq(fetcher_names::TVDB)
                || eq("Screen Grabber")
                || eq(fetcher_names::AUDIODB)
                || eq(fetcher_names::AUDIO_IMAGES)
        }),
        _ => None,
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
        images: NO_IMAGES,
    }]
}

/// The providers that fetch from an external service.
fn remote_providers() -> Vec<Provider> {
    let mut provs = metadata_providers();
    provs.extend(extractor_and_local_providers());
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
            images: Some(tmdb_images),
        },
        // OMDb's two capabilities cover DIFFERENT types upstream, so they are
        // registered as two rows: `OmdbItemProvider`/`OmdbEpisodeProvider`
        // supply metadata for Movie/Series/Episode, while
        // `OmdbImageProvider.Supports` (v10.11.8 `OmdbImageProvider.cs:75-78`)
        // is `item is Movie || item is Trailer || item is Episode` — no Series.
        Provider {
            name: fetcher_names::OMDB,
            caps: &[Cap::MetadataFetcher],
            types: &["Movie", "Series", "Episode"],
            default_enabled: true,
            compiled: true,
            images: NO_IMAGES,
        },
        Provider {
            name: fetcher_names::OMDB,
            caps: &[Cap::ImageFetcher],
            types: &["Movie", "Trailer", "Episode"],
            default_enabled: true,
            compiled: true,
            images: Some(omdb_images),
        },
        Provider {
            // Optional at runtime (needs an API key/config), like OMDb —
            // the checkbox gates; absence of config just yields no hits.
            name: fetcher_names::TVDB,
            caps: &[Cap::MetadataFetcher, Cap::ImageFetcher],
            types: &["Series", "Season", "Episode"],
            default_enabled: true,
            compiled: true,
            images: Some(tvdb_images),
        },
        Provider {
            name: fetcher_names::FANART,
            caps: &[Cap::ImageFetcher],
            types: &["Movie", "Series"],
            default_enabled: true,
            compiled: true,
            images: Some(fanart_images),
        },
        Provider {
            name: fetcher_names::MUSICBRAINZ,
            caps: &[Cap::MetadataFetcher],
            // `MusicBrainzArtistProvider` + `MusicBrainzAlbumProvider` only —
            // upstream ships no per-track (Audio) MusicBrainz provider.
            types: &["MusicArtist", "MusicAlbum"],
            default_enabled: true,
            compiled: true,
            images: NO_IMAGES,
        },
        Provider {
            name: fetcher_names::AUDIODB,
            caps: &[Cap::MetadataFetcher, Cap::ImageFetcher],
            types: &["MusicArtist", "MusicAlbum"],
            default_enabled: true,
            compiled: true,
            images: Some(audiodb_images),
        },
    ]
}

/// The image providers that read artwork out of the media file itself —
/// upstream's `EmbeddedImageProvider` (video) and `AudioImageProvider`
/// (audio) — plus the remaining non-metadata providers.
fn extractor_and_local_providers() -> Vec<Provider> {
    vec![
        Provider {
            name: fetcher_names::EMBEDDED_IMAGES,
            caps: &[Cap::ImageFetcher],
            // `EmbeddedImageProvider.Supports` is `item is Video`
            // (v10.11.8 `EmbeddedImageProvider.cs:230-242`) — audio files are
            // the separate `Image Extractor` provider below.
            types: &["Movie", "Episode", "MusicVideo", "Video"],
            default_enabled: true,
            compiled: true,
            images: Some(embedded_images),
        },
        Provider {
            name: fetcher_names::AUDIO_IMAGES,
            caps: &[Cap::ImageFetcher],
            types: &["Audio", "AudioBook"],
            default_enabled: true,
            compiled: true,
            images: Some(audio_extractor_images),
        },
        Provider {
            name: "Open Subtitles",
            caps: &[Cap::Subtitle],
            types: &["Movie", "Episode"],
            default_enabled: true,
            compiled: cfg!(feature = "opensubtitles"),
            images: NO_IMAGES,
        },
        Provider {
            name: "Local Images",
            caps: &[Cap::LocalImage],
            types: &[],
            default_enabled: true,
            compiled: true,
            images: NO_IMAGES,
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
            images: NO_IMAGES,
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
            images: NO_IMAGES,
        },
        Provider {
            name: "ListenBrainz",
            caps: &[Cap::Similarity],
            types: &["MusicArtist"],
            default_enabled: false,
            compiled: true,
            images: NO_IMAGES,
        },
        Provider {
            name: "IntroSkipper",
            caps: &[Cap::MediaSegment],
            types: &["Episode", "Movie"],
            default_enabled: true,
            compiled: true,
            images: NO_IMAGES,
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

/// `TmdbMovieImageProvider`/`TmdbSeriesImageProvider`/`TmdbSeasonImageProvider`/
/// `TmdbEpisodeImageProvider`/`TmdbBoxSetImageProvider`/`TmdbPersonImageProvider`
/// `GetSupportedImages`, keyed by the item type each one supports.
fn tmdb_images(type_name: &str) -> &'static [ImageType] {
    use ImageType::{Backdrop, Logo, Primary, Thumb};
    const TITLE: &[ImageType] = &[Primary, Backdrop, Logo, Thumb];
    const BOX_SET: &[ImageType] = &[Primary, Backdrop, Thumb];
    const PRIMARY_ONLY: &[ImageType] = &[Primary];
    match type_name {
        "Movie" | "Series" => TITLE,
        "BoxSet" => BOX_SET,
        "Season" | "Episode" | "Person" => PRIMARY_ONLY,
        _ => &[],
    }
}

/// `EmbeddedImageProvider.GetSupportedImages` (v10.11.8
/// `MediaBrowser.Providers/MediaInfo/EmbeddedImageProvider.cs:76-97`): an
/// episode yields Primary only, any other `Video` adds Backdrop and Logo, and a
/// non-video yields nothing.
fn embedded_images(type_name: &str) -> &'static [ImageType] {
    use ImageType::{Backdrop, Logo, Primary};
    const EPISODE: &[ImageType] = &[Primary];
    const VIDEO: &[ImageType] = &[Primary, Backdrop, Logo];
    match type_name {
        "Episode" => EPISODE,
        "Movie" | "MusicVideo" | "Video" => VIDEO,
        _ => &[],
    }
}

/// Constant-list `GetSupportedImages` helpers for the providers whose supported
/// set does not vary with the item type. Each list is verbatim from the C#
/// provider — the same values [`crate::provider_manager`] already keeps for the
/// remote-image search path.
fn omdb_images(_type_name: &str) -> &'static [ImageType] {
    &[ImageType::Primary]
}

/// `AudioImageProvider.GetSupportedImages` (v10.11.8
/// `MediaBrowser.Providers/MediaInfo/AudioImageProvider.cs:54-57`).
fn audio_extractor_images(_type_name: &str) -> &'static [ImageType] {
    &[ImageType::Primary]
}

/// `Jellyfin.Plugin.Tvdb`'s series/season/episode image providers.
fn tvdb_images(type_name: &str) -> &'static [ImageType] {
    use ImageType::{Backdrop, Banner, Primary};
    const SERIES: &[ImageType] = &[Primary, Banner, Backdrop];
    const PRIMARY_ONLY: &[ImageType] = &[Primary];
    match type_name {
        "Series" => SERIES,
        "Season" | "Episode" => PRIMARY_ONLY,
        _ => &[],
    }
}

/// fanart.tv's movie/series/artist/album image providers.
fn fanart_images(type_name: &str) -> &'static [ImageType] {
    use ImageType::{Art, Backdrop, Banner, Disc, Logo, Primary, Thumb};
    const MOVIE: &[ImageType] = &[Primary, Thumb, Art, Logo, Disc, Banner, Backdrop];
    const SERIES: &[ImageType] = &[Primary, Thumb, Art, Logo, Backdrop, Banner];
    const ARTIST: &[ImageType] = &[Primary, Logo, Art, Banner, Backdrop];
    const ALBUM: &[ImageType] = &[Primary, Disc];
    match type_name {
        "Movie" => MOVIE,
        "Series" => SERIES,
        "MusicArtist" => ARTIST,
        "MusicAlbum" => ALBUM,
        _ => &[],
    }
}

/// TheAudioDB's artist/album image providers.
fn audiodb_images(type_name: &str) -> &'static [ImageType] {
    use ImageType::{Backdrop, Banner, Disc, Logo, Primary};
    const ARTIST: &[ImageType] = &[Primary, Logo, Banner, Backdrop];
    const ALBUM: &[ImageType] = &[Primary, Disc];
    match type_name {
        "MusicArtist" => ARTIST,
        "MusicAlbum" => ALBUM,
        _ => &[],
    }
}

/// The image types a library of `type_name` can carry: the union of every
/// compiled image provider's `GetSupportedImages`, in provider-registration
/// order, deduplicated.
///
/// Port of `ProviderManager.AddMetadataPlugins` (v10.11.8
/// `MediaBrowser.Providers/Manager/ProviderManager.cs:706-714`):
/// `imageProviders.OfType<IRemoteImageProvider>().SelectMany(GetSupportedImages)`
/// plus the `IDynamicImageProvider`s, `.Distinct()`. Local image providers
/// (`ILocalImageProvider`) are deliberately NOT part of that union.
///
/// This replaced a hardcoded per-type-name table that advertised `Menu`,
/// `BoxRear`, `Screenshot` and `Box` for every video type — image types no
/// provider Ferrofin ships can supply.
fn supported_image_types(provs: &[Provider], type_name: &str) -> Vec<ImageType> {
    let mut types: Vec<ImageType> = Vec::new();
    for provider in provs
        .iter()
        .filter(|p| p.compiled && p.caps.contains(&Cap::ImageFetcher) && p.applies_to(type_name))
    {
        for image_type in provider.images_for(type_name) {
            if !types.contains(image_type) {
                types.push(*image_type);
            }
        }
    }
    types
}

/// Assembles the [`LibraryOptionsResultDto`] for a library whose representative
/// item types are `item_types`. `dynamic_fetchers` are runtime-registered
/// named metadata providers (WASM plugins) as (name, supported kinds) —
/// they appear in the fetcher lists exactly like compiled providers.
#[must_use]
pub fn library_options_info(
    item_types: &[String],
    is_new_library: bool,
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
            // The saver/reader lists are not per-type, so the new-library rule
            // sees the whole request (C# `IsSaverEnabledByDefault(name,
            // itemTypes, isNewLibrary)` ignores the types when isNewLibrary).
            .map(|p| p.info_for(cap, "", is_new_library))
            .collect()
    };
    let type_options = item_types
        .iter()
        .map(|type_name| {
            let per_type = |cap: Cap| -> Vec<LibraryOptionInfoDto> {
                provs
                    .iter()
                    .filter(|p| p.compiled && p.caps.contains(&cap) && p.applies_to(type_name))
                    .map(|p| p.info_for(cap, type_name, is_new_library))
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
                supported_image_types: supported_image_types(&provs, type_name),
                default_image_options: ferrofin_model::configuration::default_image_options(
                    type_name,
                )
                .to_vec(),
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
                supported_image_types: supported_image_types(&provs, type_name),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{all_metadata_plugins, library_options_info};
    use ferrofin_model::configuration::MetadataPluginType;

    use super::{image_fetcher_enabled, metadata_fetcher_enabled};
    use ferrofin_model::configuration::{LibraryOptions, TypeOptions};

    /// `BaseItemManager.IsMetadataFetcherEnabled` /
    /// `IsImageFetcherEnabled`: a saved `TypeOptions` entry is the whole
    /// answer, so an EMPTY fetcher list turns every remote provider OFF for
    /// that type — clearing the dashboard checkboxes has to mean something.
    #[test]
    fn an_empty_fetcher_list_disables_every_remote_provider() {
        let cleared = LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("Movie".to_owned()),
                metadata_fetchers: Vec::new(),
                image_fetchers: Vec::new(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(!metadata_fetcher_enabled(
            Some(&cleared),
            "Movie",
            super::fetcher_names::TMDB
        ));
        assert!(!image_fetcher_enabled(
            Some(&cleared),
            "Movie",
            super::fetcher_names::TMDB
        ));
        // A type the library never customised keeps the built-in default…
        assert!(metadata_fetcher_enabled(
            Some(&cleared),
            "Series",
            super::fetcher_names::TMDB
        ));
        // …as does a library with no saved options at all.
        assert!(metadata_fetcher_enabled(
            None,
            "Movie",
            super::fetcher_names::TMDB
        ));
    }

    #[test]
    fn a_listed_fetcher_is_enabled_case_insensitively() {
        let ticked = LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("movie".to_owned()),
                metadata_fetchers: vec!["themoviedb".to_owned()],
                image_fetchers: vec!["TheMovieDb".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(metadata_fetcher_enabled(
            Some(&ticked),
            "Movie",
            super::fetcher_names::TMDB
        ));
        assert!(image_fetcher_enabled(
            Some(&ticked),
            "Movie",
            super::fetcher_names::TMDB
        ));
        assert!(!metadata_fetcher_enabled(
            Some(&ticked),
            "Movie",
            super::fetcher_names::TVDB
        ));
    }

    #[test]
    fn movie_options_expose_real_fetchers_and_savers() {
        let info = library_options_info(&["Movie".to_owned()], false, &[]);
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
        let info = library_options_info(&["Series".to_owned(), "Episode".to_owned()], false, &[]);
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

    /// `SupportedImageTypes` is the union of the compiled image providers'
    /// `GetSupportedImages`, so it can only name types some provider can
    /// actually supply. It used to be a hardcoded 11-element enum dump that
    /// claimed Menu/BoxRear/Screenshot/Box for every video type.
    #[test]
    fn supported_image_types_come_from_the_providers() {
        use ferrofin_model::entities::ImageType;

        let info = library_options_info(
            &[
                "Movie".to_owned(),
                "Season".to_owned(),
                "Episode".to_owned(),
                "Person".to_owned(),
            ],
            false,
            &[],
        );
        let block = |name: &str| {
            info.type_options
                .iter()
                .find(|t| t.type_.as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} block"))
        };

        for absent in [
            ImageType::Menu,
            ImageType::BoxRear,
            ImageType::Screenshot,
            ImageType::Box,
        ] {
            assert!(
                !block("Movie").supported_image_types.contains(&absent),
                "no compiled provider supplies {absent:?}"
            );
        }
        // TMDb leads the registration order, so its list leads the union.
        assert_eq!(
            &block("Movie").supported_image_types[..4],
            &[
                ImageType::Primary,
                ImageType::Backdrop,
                ImageType::Logo,
                ImageType::Thumb
            ]
        );
        // TmdbSeason/TmdbEpisodeImageProvider both yield Primary only.
        assert_eq!(
            block("Season").supported_image_types,
            vec![ImageType::Primary]
        );
        // Episode also has the embedded extractor, which yields Primary there.
        assert_eq!(
            block("Episode").supported_image_types,
            vec![ImageType::Primary]
        );
        // Person: TmdbPersonImageProvider only.
        assert_eq!(
            block("Person").supported_image_types,
            vec![ImageType::Primary]
        );
    }

    /// `DefaultImageOptions` is the static `TypeOptions.DefaultImageOptions`
    /// dictionary, entry-for-entry AND in declaration order; a type the
    /// dictionary does not name gets `[]`, not a guessed Primary/Backdrop pair.
    #[test]
    fn default_image_options_are_the_csharp_table() {
        use ferrofin_model::entities::ImageType;

        let info = library_options_info(
            &[
                "Movie".to_owned(),
                "Season".to_owned(),
                "Episode".to_owned(),
                "Person".to_owned(),
                "Photo".to_owned(),
            ],
            false,
            &[],
        );
        let opts = |name: &str| {
            info.type_options
                .iter()
                .find(|t| t.type_.as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} block"))
                .default_image_options
                .iter()
                .map(|o| (o.type_, o.limit, o.min_width))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            opts("Movie"),
            vec![
                (ImageType::Backdrop, 1, 1280),
                (ImageType::Art, 0, 0),
                (ImageType::Disc, 0, 0),
                (ImageType::Primary, 1, 0),
                (ImageType::Banner, 0, 0),
                (ImageType::Thumb, 1, 0),
                (ImageType::Logo, 1, 0),
            ]
        );
        assert_eq!(
            opts("Season"),
            vec![
                (ImageType::Backdrop, 0, 1280),
                (ImageType::Primary, 1, 0),
                (ImageType::Banner, 0, 0),
                (ImageType::Thumb, 0, 0),
            ]
        );
        assert_eq!(
            opts("Episode"),
            vec![(ImageType::Backdrop, 0, 1280), (ImageType::Primary, 1, 0),]
        );
        // Not in the C# dictionary => `defaultImageOptions ?? Array.Empty<…>()`.
        assert!(opts("Person").is_empty());
        assert!(opts("Photo").is_empty());
    }

    /// `isNewLibrary=true` is the add-library wizard's pre-ticked set: no saver,
    /// and only the allowlisted fetchers.
    #[test]
    fn is_new_library_changes_the_default_enabled_set() {
        let enabled = |info: &ferrofin_model::configuration::LibraryOptionsResultDto,
                       type_name: &str,
                       image: bool,
                       name: &str| {
            let block = info
                .type_options
                .iter()
                .find(|t| t.type_.as_deref() == Some(type_name))
                .expect("block");
            let list = if image {
                &block.image_fetchers
            } else {
                &block.metadata_fetchers
            };
            list.iter()
                .find(|o| o.name.as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} listed"))
                .default_enabled
        };

        let existing = library_options_info(
            &[
                "Movie".to_owned(),
                "Series".to_owned(),
                "Episode".to_owned(),
            ],
            false,
            &[],
        );
        let fresh = library_options_info(
            &[
                "Movie".to_owned(),
                "Series".to_owned(),
                "Episode".to_owned(),
            ],
            true,
            &[],
        );

        // An existing library pre-ticks everything the built-in
        // `ServerConfiguration.MetadataOptions` blocklist does not name.
        assert!(existing.metadata_savers.iter().all(|o| o.default_enabled));
        assert!(enabled(
            &existing,
            "Movie",
            false,
            "The Open Movie Database"
        ));

        // ...and it DOES name three entries, which come back unticked even on an
        // existing library (v10.11.8 `ServerConfiguration.cs:20-63`).
        let music = library_options_info(
            &[
                "MusicAlbum".to_owned(),
                "MusicArtist".to_owned(),
                "MusicVideo".to_owned(),
            ],
            false,
            &[],
        );
        assert!(!enabled(&music, "MusicAlbum", false, "TheAudioDB"));
        assert!(!enabled(&music, "MusicArtist", false, "TheAudioDB"));
        // TheAudioDB's IMAGE capability is not on the blocklist.
        assert!(enabled(&music, "MusicAlbum", true, "TheAudioDB"));

        // A new library pre-ticks no saver at all.
        assert!(fresh.metadata_savers.iter().all(|o| !o.default_enabled));
        // TheMovieDb: metadata on for Movie/Series, off for Episode.
        assert!(enabled(&fresh, "Movie", false, "TheMovieDb"));
        assert!(enabled(&fresh, "Series", false, "TheMovieDb"));
        assert!(!enabled(&fresh, "Episode", false, "TheMovieDb"));
        // ...images on for Movie but off for Series and Episode.
        assert!(enabled(&fresh, "Movie", true, "TheMovieDb"));
        assert!(!enabled(&fresh, "Series", true, "TheMovieDb"));
        assert!(!enabled(&fresh, "Episode", true, "TheMovieDb"));
        // OMDb is not on either allowlist.
        assert!(!enabled(&fresh, "Movie", false, "The Open Movie Database"));
        assert!(!enabled(&fresh, "Movie", true, "The Open Movie Database"));
        // TheTVDB is on both.
        assert!(enabled(&fresh, "Series", false, "TheTVDB"));
        assert!(enabled(&fresh, "Series", true, "TheTVDB"));
        // The VIDEO-side extractor is NOT on the allowlist; the audio-side
        // "Image Extractor" is.
        assert!(!enabled(&fresh, "Movie", true, "Embedded Image Extractor"));
        let fresh_music = library_options_info(&["Audio".to_owned()], true, &[]);
        assert!(enabled(&fresh_music, "Audio", true, "Image Extractor"));
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
