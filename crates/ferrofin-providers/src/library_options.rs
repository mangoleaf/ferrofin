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
//! always compiled (shared API keys are supplied where required);
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

/// Whether metadata fetcher `name` is enabled for item type `kind`.
///
/// Port of `BaseItemManager.IsMetadataFetcherEnabled` (upstream master
/// `MediaBrowser.Controller/BaseItemManager/BaseItemManager.cs:26-47`): when
/// the item's library saved a `TypeOptions` entry for the kind, the answer is
/// exactly `libraryTypeOptions.MetadataFetchers.Contains(name,
/// OrdinalIgnoreCase)` — so an EMPTY list disables every remote fetcher.
/// With no entry (the library never customised that type, or the item is in
/// no library at all — a by-name artist, which `GetLibraryOptions` gives a
/// bare `new LibraryOptions()`, `LibraryManager.cs:2884-2896`), the SERVER-WIDE options for the kind decide
/// (`:45-46`): `itemConfig is null ||
/// !itemConfig.DisabledMetadataFetchers.Contains(name)`. `global` is that
/// server-wide entry ([`global_metadata_options`]); `None` when the server
/// configuration names none for the kind (every fetcher enabled).
///
/// This is the ONE gate: C# routes both the scan and the on-demand
/// `POST /Items/{id}/Refresh` through `ProviderManager.CanRefreshMetadata`,
/// which calls it. Anything in Ferrofin that fetches remote metadata must ask
/// here too, or clearing the checkboxes stops meaning anything.
///
/// TODO(parity, open work item — NOT an accepted divergence): the two
/// `// Hack alert.` arms at `BaseItemManager.cs:28-38` are not ported: a
/// `Channel` item is always enabled, and an item whose `SourceType` is
/// `Channel` is enabled iff `!EnableMediaSourceDisplay`. Neither is reachable
/// from this signature, which takes a kind and not the item; porting them
/// means passing the item's kind and `SourceType` here from both callers.
#[must_use]
pub fn metadata_fetcher_enabled(
    options: Option<&ferrofin_model::configuration::LibraryOptions>,
    global: Option<&ferrofin_model::configuration::MetadataOptions>,
    kind: &str,
    name: &str,
) -> bool {
    match type_entry(options, kind) {
        Some(t) => t
            .metadata_fetchers
            .iter()
            .any(|f| f.eq_ignore_ascii_case(name)),
        None => global.is_none_or(|g| {
            !g.disabled_metadata_fetchers
                .iter()
                .any(|f| f.eq_ignore_ascii_case(name))
        }),
    }
}

/// The admin-chosen order position of metadata fetcher `name` for item type
/// `kind` — lower sorts first, and a fetcher absent from the list sorts last.
///
/// Port of `ProviderManager.GetConfiguredOrder` over
/// `typeOptions?.MetadataFetcherOrder ?? globalMetadataOptions.MetadataFetcherOrder`
/// (`ProviderManager.cs:523-525`): the library's order when it saved a
/// `TypeOptions` entry for the kind, else the server-wide one (`global`).
/// `Array.IndexOf(order, providerName)` returns `-1` → `int.MaxValue` for a
/// fetcher the admin never ranked. Order is load-bearing wherever results from
/// several fetchers are merged first-writer-wins: `GetRemoteSearchResults`
/// dedups candidates by shared provider id, so the highest-ranked fetcher
/// decides which record a client sees.
#[must_use]
pub fn metadata_fetcher_rank(
    options: Option<&ferrofin_model::configuration::LibraryOptions>,
    global: Option<&ferrofin_model::configuration::MetadataOptions>,
    kind: &str,
    name: &str,
) -> usize {
    let order = match type_entry(options, kind) {
        Some(t) => t.metadata_fetcher_order.as_slice(),
        None => global.map_or(&[][..], |g| g.metadata_fetcher_order.as_slice()),
    };
    configured_order(order, name)
}

/// [`metadata_fetcher_rank`] for the image fetchers:
/// `typeOptions?.ImageFetcherOrder ?? options.ImageFetcherOrder`
/// (`ProviderManager.cs:405-406`).
#[must_use]
pub fn image_fetcher_rank(
    options: Option<&ferrofin_model::configuration::LibraryOptions>,
    global: Option<&ferrofin_model::configuration::MetadataOptions>,
    kind: &str,
    name: &str,
) -> usize {
    let order = match type_entry(options, kind) {
        Some(t) => t.image_fetcher_order.as_slice(),
        None => global.map_or(&[][..], |g| g.image_fetcher_order.as_slice()),
    };
    configured_order(order, name)
}

/// `ProviderManager.GetDefaultOrder` (`ProviderManager.cs:630-640`) for a
/// provider that declares no `IHasOrder`: "after items that want to be first
/// (~0) but before items that want to be last (~100)". TheMovieDb's season,
/// person and box-set providers, every TheTVDB provider and every WASM
/// plugin's metadata source rank by it.
pub const DEFAULT_ORDER: i32 = 50;

/// `GetDefaultOrder` of built-in metadata fetcher `name` for item type `kind`
/// (`ProviderManager.cs:630-640`): its `IHasOrder.Order`, else
/// [`DEFAULT_ORDER`]. Upstream master `96bca6f0bd`: `TmdbMovieProvider`,
/// `TmdbSeriesProvider` and `TmdbEpisodeProvider` declare 1 (the season,
/// person and box-set providers nothing); `OmdbItemProvider` 2 and
/// `OmdbEpisodeProvider` 1; `MusicBrainzAlbumProvider`/
/// `MusicBrainzArtistProvider` 0 and `AudioDbAlbumProvider`/
/// `AudioDbArtistProvider` 1; the TVDB plugin's providers nothing.
#[must_use]
pub fn default_metadata_order(name: &str, kind: &str) -> i32 {
    match name {
        fetcher_names::TMDB if matches!(kind, "Movie" | "Series" | "Episode") => 1,
        fetcher_names::OMDB if kind == "Episode" => 1,
        fetcher_names::OMDB => 2,
        fetcher_names::MUSICBRAINZ => 0,
        fetcher_names::AUDIODB => 1,
        _ => DEFAULT_ORDER,
    }
}

/// The built-in remote metadata providers in Ferrofin's registration order —
/// the last key of the provider order, which breaks a FULL tie (the same
/// saved rank, or none, and the same `IHasOrder`).
///
/// Ferrofin's tie rule has three categories where Jellyfin has two:
/// **WASM plugins first (in load order), compiled-in extensions second,
/// built-in providers third** (owner decision, 2026-10-04). Jellyfin
/// registers its plugins' assemblies before the server's own
/// (`ApplicationHost.GetComposablePartAssemblies:881-886` →
/// `GetExportTypes` → `ProviderManager.AddParts:163`), and its provider
/// order is a stable `OrderBy(GetConfiguredOrder).ThenBy(GetDefaultOrder)`,
/// so a plugin's provider wins a full tie with a server one. Ferrofin keeps
/// that for the third-party code it loads (WASM plugins), places the
/// plugins it compiled in (extensions — none provides a metadata source
/// today) after them, and its built-ins last. Among the built-ins TheTVDB
/// registers first, because in Jellyfin it is a plugin: a season with no
/// saved order asks TheTVDB before TheMovieDb (both declare no
/// `IHasOrder`), as Jellyfin does. TheMovieDb then precedes OMDb, which
/// breaks their episode tie (both 1) as Ferrofin always has (upstream's
/// order there is type discovery within one assembly, which its source does
/// not fix).
pub const BUILT_IN_METADATA_FETCHERS: &[&str] = &[
    fetcher_names::TVDB,
    fetcher_names::TMDB,
    fetcher_names::OMDB,
    fetcher_names::MUSICBRAINZ,
    fetcher_names::AUDIODB,
];

/// Default order for built-in artwork providers. Explicit library order is
/// applied before this tie-breaker. Fanart leads movie and series artwork,
/// and precedes AudioDB for music, with OMDb as the final poster fallback.
#[must_use]
pub fn default_image_order(name: &str) -> usize {
    match name {
        fetcher_names::TMDB | fetcher_names::AUDIODB => 1,
        fetcher_names::TVDB => 50,
        fetcher_names::OMDB => 90,
        _ => 0,
    }
}

/// `GetConfiguredOrder` (`ProviderManager.cs:617-628`): the position of
/// `name` in `order`, or last. Unlike enable lists, order uses exact casing.
pub(crate) fn configured_order(order: &[String], name: &str) -> usize {
    order.iter().position(|f| f == name).unwrap_or(usize::MAX)
}

/// The library's configured `MetadataFetcherOrder` for item type `kind`, or
/// `None` when the library saved no `TypeOptions` entry for the kind.
///
/// The `None` is load-bearing and is why this is not a bare `Vec`:
/// `GetMetadataProvidersInternal` reads
/// `typeOptions?.MetadataFetcherOrder ?? globalMetadataOptions.MetadataFetcherOrder`
/// (`ProviderManager.cs:445`), and `??` fires on a MISSING entry only. A saved
/// entry whose order list is empty is an answer — "this library ranks nothing"
/// — and must NOT fall through to the server-wide order, or clearing the list
/// in the UI would silently re-inherit the global one.
#[must_use]
pub fn metadata_fetcher_order(
    options: Option<&ferrofin_model::configuration::LibraryOptions>,
    kind: &str,
) -> Option<Vec<String>> {
    type_entry(options, kind).map(|t| t.metadata_fetcher_order.clone())
}

/// The server-wide [`MetadataOptions`] entry for item type `kind`.
///
/// Port of `MetadataConfigurationExtensions.GetMetadataOptionsForType`
/// (v10.11.8 `MediaBrowser.Controller/Library/MetadataConfigurationExtensions.cs:21`):
/// `Array.Find(config.MetadataOptions, i => i.ItemType == type)`, ordinal
/// case-insensitive, `null` when the server configuration names no entry for
/// the type.
#[must_use]
pub fn global_metadata_options<'a>(
    all: &'a [ferrofin_model::configuration::MetadataOptions],
    kind: &str,
) -> Option<&'a ferrofin_model::configuration::MetadataOptions> {
    all.iter().find(|o| {
        o.item_type
            .as_deref()
            .is_some_and(|t| t.eq_ignore_ascii_case(kind))
    })
}

/// Whether image fetcher `name` is enabled for item type `kind`.
///
/// Port of `BaseItemManager.IsImageFetcherEnabled` (`BaseItemManager.cs:
/// 50-71`), the image half of the same gate (`ProviderManager.
/// CanRefreshImages`): the library's `TypeOptions.ImageFetchers` when it saved
/// an entry for the kind, else the server-wide `DisabledImageFetchers`
/// (`global`), as [`metadata_fetcher_enabled`] does.
#[must_use]
pub fn image_fetcher_enabled(
    options: Option<&ferrofin_model::configuration::LibraryOptions>,
    global: Option<&ferrofin_model::configuration::MetadataOptions>,
    kind: &str,
    name: &str,
) -> bool {
    match type_entry(options, kind) {
        Some(t) => t
            .image_fetchers
            .iter()
            .any(|f| f.eq_ignore_ascii_case(name)),
        None => global.is_none_or(|g| {
            !g.disabled_image_fetchers
                .iter()
                .any(|f| f.eq_ignore_ascii_case(name))
        }),
    }
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
    /// OMDb movie/series/episode metadata (the Rotten Tomatoes rating among
    /// it) and posters.
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
    /// Upstream's `VideoImageProvider` (frame grabs), which Ferrofin does
    /// not register. Its name is on the new-library image allowlist
    /// (`IsImageFetcherEnabledByDefault`), so it is reserved: a plugin
    /// calling itself that would start ticked in a new library.
    pub const SCREEN_GRABBER: &str = "Screen Grabber";

    /// Every built-in fetcher name — the reserved set a dynamically
    /// registered (WASM) provider name must not collide with: a plugin
    /// declaring `"TheMovieDb"` would ride TMDB's checkbox/order and
    /// appear twice in the dashboard lists, and one declaring a name on a
    /// new-library allowlist would start ticked there.
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
        SCREEN_GRABBER,
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
    /// through the C# helpers ([`default_enabled_for`]) rather than the
    /// registry's standing default.
    fn info_for(&self, cap: Cap, type_name: &str, defaults: Defaults<'_>) -> LibraryOptionInfoDto {
        LibraryOptionInfoDto {
            name: Some(self.name.to_owned()),
            default_enabled: default_enabled_for(self.name, cap, type_name, defaults)
                .unwrap_or(self.default_enabled),
        }
    }
    /// The image types this provider supplies for `type_name`.
    fn images_for(&self, type_name: &str) -> &'static [ImageType] {
        self.images.map_or(&[][..], |f| f(type_name))
    }
}

/// What a library-options request's `DefaultEnabled` flags depend on
/// besides the provider: whether the library is new (the `isNewLibrary`
/// query flag), the request's representative item types, and the server's
/// per-type `MetadataOptions` (`ServerConfiguration.MetadataOptions`, read
/// live by the caller; it carries the constructor's built-in `Disabled*`
/// defaults — `ServerConfiguration.cs:20-63`, Ferrofin's
/// `default_metadata_options` — plus any admin edit).
#[derive(Clone, Copy)]
struct Defaults<'a> {
    is_new_library: bool,
    item_types: &'a [String],
    global: &'a [ferrofin_model::configuration::MetadataOptions],
}

/// The `DefaultEnabled` a fetcher/saver reports, ported from
/// `LibraryController.IsSaverEnabledByDefault` /
/// `IsMetadataFetcherEnabledByDefault` / `IsImageFetcherEnabledByDefault`
/// (`LibraryController.cs:1033-1087`, master `96bca6f0bd`).
///
/// `None` means "the C# helper does not apply to this capability", leaving the
/// registry default in place.
///
/// In a new library the helpers tick only their allowlists. Otherwise they
/// read the server's `MetadataOptions`: a saver is ticked when no entry
/// names one of the request's item types, or any of those entries does not
/// disable it (`metadataOptions.Length == 0 || metadataOptions.Any(i =>
/// !i.DisabledMetadataSavers.Contains(name))`); a fetcher when the type's
/// entry (`GetMetadataOptionsForType`) is missing or does not list it in
/// `DisabledMetadataFetchers` / `DisabledImageFetchers` — the same
/// server-wide lists the scan's gate falls back to
/// ([`metadata_fetcher_enabled`], [`image_fetcher_enabled`]), so the
/// dashboard shows unticked what the scan would not run.
fn default_enabled_for(
    name: &str,
    cap: Cap,
    type_name: &str,
    defaults: Defaults<'_>,
) -> Option<bool> {
    let Defaults {
        is_new_library,
        item_types,
        global,
    } = defaults;
    if !is_new_library {
        let names = |list: &[String]| list.iter().any(|n| n.eq_ignore_ascii_case(name));
        return match cap {
            Cap::MetadataSaver => {
                let entries: Vec<_> = global
                    .iter()
                    .filter(|o| {
                        o.item_type
                            .as_deref()
                            .is_some_and(|t| item_types.iter().any(|i| i.eq_ignore_ascii_case(t)))
                    })
                    .collect();
                Some(
                    entries.is_empty()
                        || entries.iter().any(|o| !names(&o.disabled_metadata_savers)),
                )
            }
            Cap::MetadataFetcher => Some(
                global_metadata_options(global, type_name)
                    .is_none_or(|o| !names(&o.disabled_metadata_fetchers)),
            ),
            Cap::ImageFetcher => Some(
                global_metadata_options(global, type_name)
                    .is_none_or(|o| !names(&o.disabled_image_fetchers)),
            ),
            _ => None,
        };
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
            // name is kept so the arm stays a faithful transliteration (and is
            // reserved against plugins, `fetcher_names::ALL`).
            eq(fetcher_names::TVDB)
                || eq(fetcher_names::SCREEN_GRABBER)
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
            // Uses a built-in project key; the library checkbox gates requests.
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

/// `Jellyfin.Plugin.Tvdb`'s series/season/episode image providers'
/// `GetSupportedImages` (`TvdbSeriesImageProvider.cs:59-66`,
/// `TvdbSeasonImageProvider.cs:59-64`, `TvdbEpisodeImageProvider.cs:49-52`).
///
/// Season acquisition follows the selected display order and uses the season
/// artwork reference types (`TvdbClient::season_images`) in scan and refresh.
fn tvdb_images(type_name: &str) -> &'static [ImageType] {
    use ImageType::{Art, Backdrop, Banner, Logo, Primary};
    const SERIES: &[ImageType] = &[Primary, Banner, Backdrop, Logo, Art];
    const SEASON: &[ImageType] = &[Primary, Banner, Backdrop];
    const EPISODE: &[ImageType] = &[Primary];
    match type_name {
        "Series" => SERIES,
        "Season" => SEASON,
        "Episode" => EPISODE,
        _ => &[],
    }
}

/// fanart.tv's movie/series/artist/album image providers.
///
/// TODO(parity, open work item): the plugin's `SeasonProvider` (Backdrop,
/// Thumb, Banner, Primary — `SeasonProvider.cs:51-60`) is not advertised for
/// a Season because nothing fetches it yet: port its `GetImages` (the
/// series' fanart JSON filtered to the season's number) into the season
/// image pass, then add `"Season"` to fanart's registry types and its list
/// here.
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
///
/// Each type's `MetadataFetchers` come in the order a library that saved no
/// order runs them (`GetPluginSummary` → `AddMetadataPlugins` →
/// `GetMetadataProvidersInternal` over `new LibraryOptions()` and the
/// server-wide options, `ProviderManager.cs:657-760`): the server-wide
/// `MetadataFetcherOrder` for the type (`global`), then each provider's
/// `IHasOrder` ([`default_metadata_order`]; a WASM plugin's is
/// [`DEFAULT_ORDER`]), then registration — the plugins first, then the
/// built-ins in [`BUILT_IN_METADATA_FETCHERS`] order. jellyfin-web shows a
/// new library's fetchers in this order and saves it as the library's
/// `MetadataFetcherOrder` (`libraryoptionseditor.js` `getOrderedPlugins`,
/// `setMetadataFetchersIntoOptions`), so the order a new library is shown is
/// the order its scans run.
#[must_use]
pub fn library_options_info(
    item_types: &[String],
    is_new_library: bool,
    dynamic_fetchers: &[(String, Vec<String>)],
    global: &[ferrofin_model::configuration::MetadataOptions],
) -> LibraryOptionsResultDto {
    let defaults = Defaults {
        is_new_library,
        item_types,
        global,
    };
    // A plugin's fetcher takes `DefaultEnabled` from the same port of
    // `IsMetadataFetcherEnabledByDefault` / `IsImageFetcherEnabledByDefault`
    // (`LibraryController.cs:1047-1087`) as a built-in one: in a new library
    // only the allowlisted built-ins are ticked, so a plugin starts
    // unticked, as Jellyfin shows a plugin's provider; otherwise the
    // server's `Disabled*` lists decide, as for any fetcher.
    let dynamic_info = |name: &str, cap: Cap, type_name: &str| LibraryOptionInfoDto {
        name: Some(name.to_owned()),
        default_enabled: default_enabled_for(name, cap, type_name, defaults).unwrap_or(true),
    };
    let provs = providers();
    let flat = |cap: Cap| -> Vec<LibraryOptionInfoDto> {
        provs
            .iter()
            .filter(|p| p.compiled && p.caps.contains(&cap))
            // The saver/reader lists are not per-type, so the saver rule sees
            // the whole request (C# `IsSaverEnabledByDefault(name, itemTypes,
            // isNewLibrary)`: no saver in a new library, else the server's
            // entries for the request's item types).
            .map(|p| p.info_for(cap, "", defaults))
            .collect()
    };
    let type_options = item_types
        .iter()
        .map(|type_name| {
            let per_type = |cap: Cap| -> Vec<LibraryOptionInfoDto> {
                provs
                    .iter()
                    .filter(|p| p.compiled && p.caps.contains(&cap) && p.applies_to(type_name))
                    .map(|p| p.info_for(cap, type_name, defaults))
                    .collect()
            };
            let mut metadata_fetchers: Vec<LibraryOptionInfoDto> = dynamic_fetchers
                .iter()
                .filter(|(_, kinds)| kinds.iter().any(|k| k == type_name))
                .map(|(name, _)| dynamic_info(name, Cap::MetadataFetcher, type_name))
                .collect();
            let plugins = metadata_fetchers.len();
            metadata_fetchers.extend(per_type(Cap::MetadataFetcher));
            let order = global_metadata_options(global, type_name)
                .map_or(&[][..], |g| g.metadata_fetcher_order.as_slice());
            // Stable, so the plugins keep their load order among themselves.
            let mut keyed: Vec<(usize, LibraryOptionInfoDto)> =
                metadata_fetchers.into_iter().enumerate().collect();
            keyed.sort_by_key(|(position, info)| {
                let name = info.name.as_deref().unwrap_or_default();
                let registration = if *position < plugins {
                    *position
                } else {
                    plugins
                        + BUILT_IN_METADATA_FETCHERS
                            .iter()
                            .position(|n| n.eq_ignore_ascii_case(name))
                            .unwrap_or(BUILT_IN_METADATA_FETCHERS.len())
                };
                let default_order = if *position < plugins {
                    DEFAULT_ORDER
                } else {
                    default_metadata_order(name, type_name)
                };
                (configured_order(order, name), default_order, registration)
            });
            let metadata_fetchers: Vec<LibraryOptionInfoDto> =
                keyed.into_iter().map(|(_, info)| info).collect();
            let mut image_fetchers = per_type(Cap::ImageFetcher);
            image_fetchers.extend(
                dynamic_fetchers
                    .iter()
                    .filter(|(_, kinds)| kinds.iter().any(|k| k == type_name))
                    .map(|(name, _)| dynamic_info(name, Cap::ImageFetcher, type_name)),
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
            None,
            "Movie",
            super::fetcher_names::TMDB
        ));
        assert!(!image_fetcher_enabled(
            Some(&cleared),
            None,
            "Movie",
            super::fetcher_names::TMDB
        ));
        // A type the library never customised keeps the built-in default…
        assert!(metadata_fetcher_enabled(
            Some(&cleared),
            None,
            "Series",
            super::fetcher_names::TMDB
        ));
        // …as does a library with no saved options at all.
        assert!(metadata_fetcher_enabled(
            None,
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
            None,
            "Movie",
            super::fetcher_names::TMDB
        ));
        assert!(image_fetcher_enabled(
            Some(&ticked),
            None,
            "Movie",
            super::fetcher_names::TMDB
        ));
        assert!(!metadata_fetcher_enabled(
            Some(&ticked),
            None,
            "Movie",
            super::fetcher_names::TVDB
        ));
    }

    /// `IsMetadataFetcherEnabled`/`IsImageFetcherEnabled` with no library
    /// `TypeOptions` entry for the kind — a library that never customised
    /// it, or an item in no library (a by-name artist, `new LibraryOptions()`)
    /// — fall back to the server-wide `MetadataOptions` for the kind
    /// (`BaseItemManager.cs:45-46,69-70`); a saved library entry still wins
    /// over them.
    #[test]
    fn without_a_library_entry_the_server_wide_options_decide() {
        use ferrofin_model::configuration::MetadataOptions;
        // The `ServerConfiguration` constructor's MusicArtist entry.
        let server = MetadataOptions {
            item_type: Some("MusicArtist".to_owned()),
            disabled_metadata_fetchers: vec!["TheAudioDB".to_owned()],
            disabled_image_fetchers: vec!["FanArt".to_owned()],
            ..MetadataOptions::default()
        };
        let audiodb = super::fetcher_names::AUDIODB;
        let musicbrainz = super::fetcher_names::MUSICBRAINZ;
        assert!(!metadata_fetcher_enabled(
            None,
            Some(&server),
            "MusicArtist",
            audiodb
        ));
        assert!(metadata_fetcher_enabled(
            None,
            Some(&server),
            "MusicArtist",
            musicbrainz
        ));
        assert!(!image_fetcher_enabled(
            None,
            Some(&server),
            "MusicArtist",
            super::fetcher_names::FANART
        ));
        assert!(image_fetcher_enabled(
            None,
            Some(&server),
            "MusicArtist",
            audiodb
        ));
        // A library that saved an entry for the kind is the whole answer.
        let library = LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("MusicArtist".to_owned()),
                metadata_fetchers: vec!["TheAudioDB".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(metadata_fetcher_enabled(
            Some(&library),
            Some(&server),
            "MusicArtist",
            audiodb
        ));
        assert!(!metadata_fetcher_enabled(
            Some(&library),
            Some(&server),
            "MusicArtist",
            musicbrainz
        ));
    }

    /// A type's `MetadataFetchers` come in the order a library with no saved
    /// order runs them, which jellyfin-web shows a new library and saves as
    /// its `MetadataFetcherOrder`: the server-wide order, then `IHasOrder`
    /// (TheMovieDb 1 for movies, series and episodes, OMDb 2 / 1 for
    /// episodes, MusicBrainz 0, TheAudioDB 1, 50 for the rest and every
    /// plugin), then registration — plugins first, then TheTVDB, TheMovieDb,
    /// OMDb, MusicBrainz, TheAudioDB.
    #[test]
    fn metadata_fetchers_are_listed_in_the_order_they_run() {
        use super::fetcher_names::{AUDIODB, MUSICBRAINZ, OMDB, TMDB, TVDB};
        let names = |info: &ferrofin_model::configuration::LibraryOptionsResultDto, kind: &str| {
            info.type_options
                .iter()
                .find(|t| t.type_.as_deref() == Some(kind))
                .expect("the type")
                .metadata_fetchers
                .iter()
                .filter_map(|f| f.name.clone())
                .collect::<Vec<_>>()
        };
        let kinds: Vec<String> = ["Movie", "Series", "Season", "Episode", "MusicAlbum"]
            .iter()
            .map(|k| (*k).to_owned())
            .collect();
        let plugin = (
            "PluginDb".to_owned(),
            vec![
                "Movie".to_owned(),
                "Series".to_owned(),
                "Season".to_owned(),
                "MusicAlbum".to_owned(),
            ],
        );
        let builtin = library_options_info(&kinds, true, &[], &[]);
        assert_eq!(names(&builtin, "Movie"), [TMDB, OMDB]);
        assert_eq!(names(&builtin, "Series"), [TMDB, OMDB, TVDB]);
        assert_eq!(
            names(&builtin, "Season"),
            [TVDB, TMDB],
            "both 50: TheTVDB registers first, a plugin in Jellyfin"
        );
        assert_eq!(
            names(&builtin, "Episode"),
            [TMDB, OMDB, TVDB],
            "TheMovieDb and OMDb both 1: TheMovieDb registers first"
        );
        assert_eq!(names(&builtin, "MusicAlbum"), [MUSICBRAINZ, AUDIODB]);
        let with_plugin = library_options_info(&kinds, true, std::slice::from_ref(&plugin), &[]);
        assert_eq!(names(&with_plugin, "Movie"), [TMDB, OMDB, "PluginDb"]);
        assert_eq!(
            names(&with_plugin, "Series"),
            [TMDB, OMDB, "PluginDb", TVDB],
            "a full tie at 50: the plugin registers before every built-in"
        );
        assert_eq!(names(&with_plugin, "Season"), ["PluginDb", TVDB, TMDB]);
        assert_eq!(names(&with_plugin, "Episode"), [TMDB, OMDB, TVDB]);
        assert_eq!(
            names(&with_plugin, "MusicAlbum"),
            [MUSICBRAINZ, AUDIODB, "PluginDb"]
        );
        let server = [ferrofin_model::configuration::MetadataOptions {
            item_type: Some("Series".to_owned()),
            metadata_fetcher_order: vec![TVDB.to_owned(), "PluginDb".to_owned()],
            ..Default::default()
        }];
        let ordered = library_options_info(&kinds, false, &[plugin], &server);
        assert_eq!(
            names(&ordered, "Series"),
            [TVDB, "PluginDb", TMDB, OMDB],
            "the server-wide order ranks first, the rest by IHasOrder"
        );
        assert_eq!(names(&ordered, "Season"), ["PluginDb", TVDB, TMDB]);
    }

    /// A plugin's fetcher takes `DefaultEnabled` from upstream's
    /// `IsMetadataFetcherEnabledByDefault` / `IsImageFetcherEnabledByDefault`
    /// (`LibraryController.cs:1047-1087`), as a built-in's does: in a new
    /// library a name outside their allowlists starts unticked, so
    /// jellyfin-web saves the plugin listed but not checked (in a TV library
    /// it is listed first for seasons, where everything declares 50, yet
    /// never runs until the admin ticks it); in an existing library the
    /// server's `Disabled*` lists decide, and they name no plugin.
    #[test]
    fn a_plugin_fetcher_starts_unticked_in_a_new_library() {
        let kinds: Vec<String> = ["Series", "Season", "Episode"]
            .iter()
            .map(|k| (*k).to_owned())
            .collect();
        let plugin = (
            "PluginDb".to_owned(),
            vec!["Series".to_owned(), "Season".to_owned()],
        );
        let ticked = |info: &ferrofin_model::configuration::LibraryOptionsResultDto,
                      kind: &str,
                      images: bool| {
            let entry = info
                .type_options
                .iter()
                .find(|t| t.type_.as_deref() == Some(kind))
                .expect("the type");
            let list = if images {
                &entry.image_fetchers
            } else {
                &entry.metadata_fetchers
            };
            list.iter()
                .find(|f| f.name.as_deref() == Some("PluginDb"))
                .expect("the plugin is listed")
                .default_enabled
        };
        let fresh = library_options_info(&kinds, true, std::slice::from_ref(&plugin), &[]);
        for kind in ["Series", "Season"] {
            assert!(!ticked(&fresh, kind, false), "{kind} metadata, new library");
            assert!(!ticked(&fresh, kind, true), "{kind} images, new library");
        }
        let season = &fresh
            .type_options
            .iter()
            .find(|t| t.type_.as_deref() == Some("Season"))
            .expect("Season")
            .metadata_fetchers;
        assert_eq!(season[0].name.as_deref(), Some("PluginDb"), "listed first");
        assert!(
            season
                .iter()
                .any(|f| f.name.as_deref() == Some(super::fetcher_names::TVDB) && f.default_enabled),
            "TheTVDB is ticked for seasons in a new library"
        );
        let existing = library_options_info(&kinds, false, &[plugin], &[]);
        for kind in ["Series", "Season"] {
            assert!(ticked(&existing, kind, false), "{kind} metadata, existing");
            assert!(ticked(&existing, kind, true), "{kind} images, existing");
        }
    }

    #[test]
    fn movie_options_expose_real_fetchers_and_savers() {
        let info = library_options_info(&["Movie".to_owned()], false, &[], &[]);
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
        let info = library_options_info(
            &["Series".to_owned(), "Episode".to_owned()],
            false,
            &[],
            &[],
        );
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
        // TmdbSeasonImageProvider yields Primary; TheTVDB's season provider
        // adds Banner and Backdrop (`TvdbSeasonImageProvider.cs:59-64`).
        assert_eq!(
            block("Season").supported_image_types,
            vec![ImageType::Primary, ImageType::Banner, ImageType::Backdrop]
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

    /// The per-type `MetadataOptions` the C# `ServerConfiguration`
    /// constructor seeds (`ServerConfiguration.cs:20-63`; Ferrofin's
    /// `configuration_manager::default_metadata_options`): OMDb disabled for
    /// music videos (metadata and images), TheAudioDB's metadata for albums
    /// and artists.
    fn server_defaults() -> Vec<ferrofin_model::configuration::MetadataOptions> {
        use ferrofin_model::configuration::MetadataOptions;
        let entry = |kind: &str, fetchers: &[&str], images: &[&str]| MetadataOptions {
            item_type: Some(kind.to_owned()),
            disabled_metadata_fetchers: fetchers.iter().map(|n| (*n).to_owned()).collect(),
            disabled_image_fetchers: images.iter().map(|n| (*n).to_owned()).collect(),
            ..MetadataOptions::default()
        };
        vec![
            entry("Book", &[], &[]),
            entry("Movie", &[], &[]),
            entry(
                "MusicVideo",
                &["The Open Movie Database"],
                &["The Open Movie Database"],
            ),
            entry("Series", &[], &[]),
            entry("MusicAlbum", &["TheAudioDB"], &[]),
            entry("MusicArtist", &["TheAudioDB"], &[]),
            entry("BoxSet", &[], &[]),
            entry("Season", &[], &[]),
            entry("Episode", &[], &[]),
        ]
    }

    /// In an existing library a fetcher or saver is ticked unless the
    /// server's `MetadataOptions` disable it (`IsMetadataFetcherEnabledByDefault`
    /// / `IsImageFetcherEnabledByDefault` / `IsSaverEnabledByDefault`'s
    /// non-new branches, read live): an admin's `Disabled*` entry unticks it,
    /// a plugin included, as the scan's gate would not run it; a type the
    /// configuration names no entry for ticks everything.
    #[test]
    fn the_servers_disabled_lists_untick_a_fetcher_in_an_existing_library() {
        use ferrofin_model::configuration::MetadataOptions;
        let global = [MetadataOptions {
            item_type: Some("Movie".to_owned()),
            disabled_metadata_fetchers: vec!["themoviedb".to_owned(), "PluginDb".to_owned()],
            disabled_image_fetchers: vec!["FanArt".to_owned()],
            disabled_metadata_savers: vec!["Nfo".to_owned()],
            ..MetadataOptions::default()
        }];
        let plugin = ("PluginDb".to_owned(), vec!["Movie".to_owned()]);
        let ticked = |info: &ferrofin_model::configuration::LibraryOptionsResultDto,
                      images: bool,
                      name: &str| {
            let entry = &info.type_options[0];
            let list = if images {
                &entry.image_fetchers
            } else {
                &entry.metadata_fetchers
            };
            list.iter()
                .find(|f| f.name.as_deref() == Some(name))
                .unwrap_or_else(|| panic!("{name} listed"))
                .default_enabled
        };
        let movie = ["Movie".to_owned()];
        let info = library_options_info(&movie, false, std::slice::from_ref(&plugin), &global);
        assert!(
            !ticked(&info, false, "TheMovieDb"),
            "disabled, case-insensitively"
        );
        assert!(
            !ticked(&info, false, "PluginDb"),
            "a plugin the admin disabled"
        );
        assert!(ticked(&info, false, "The Open Movie Database"));
        assert!(!ticked(&info, true, "FanArt"));
        assert!(
            ticked(&info, true, "TheMovieDb"),
            "images are their own list"
        );
        assert!(
            info.metadata_savers
                .iter()
                .all(|o| o.name.as_deref() != Some("Nfo") || !o.default_enabled),
            "the only entry for the request's types disables the NFO saver"
        );
        let unnamed = library_options_info(&movie, false, &[plugin], &[]);
        assert!(ticked(&unnamed, false, "TheMovieDb"), "no entry: ticked");
        assert!(ticked(&unnamed, false, "PluginDb"));
        assert!(unnamed.metadata_savers.iter().all(|o| o.default_enabled));
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
            &server_defaults(),
        );
        let fresh = library_options_info(
            &[
                "Movie".to_owned(),
                "Series".to_owned(),
                "Episode".to_owned(),
            ],
            true,
            &[],
            &[],
        );

        // An existing library pre-ticks everything the server's
        // `MetadataOptions` (here the constructor's defaults) do not disable.
        assert!(existing.metadata_savers.iter().all(|o| o.default_enabled));
        assert!(enabled(
            &existing,
            "Movie",
            false,
            "The Open Movie Database"
        ));

        // ...and the defaults DO disable three entries, which come back
        // unticked even on an existing library (`ServerConfiguration.cs:20-63`).
        let music = library_options_info(
            &[
                "MusicAlbum".to_owned(),
                "MusicArtist".to_owned(),
                "MusicVideo".to_owned(),
            ],
            false,
            &[],
            &server_defaults(),
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
        let fresh_music = library_options_info(&["Audio".to_owned()], true, &[], &[]);
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

    /// `typeOptions?.MetadataFetcherOrder ?? globalMetadataOptions.
    /// MetadataFetcherOrder` (`ProviderManager.cs:523-525`) and its image
    /// twin (`:405-406`): a library's saved entry ranks — an empty list
    /// ranks nothing and does not fall back — and without one the
    /// server-wide order does.
    #[test]
    fn fetcher_ranks_fall_back_to_the_server_wide_order_only_without_an_entry() {
        use super::{image_fetcher_rank, metadata_fetcher_rank};
        use ferrofin_model::configuration::MetadataOptions;
        let global = MetadataOptions {
            item_type: Some("MusicAlbum".to_owned()),
            metadata_fetcher_order: vec!["TheAudioDB".to_owned(), "MusicBrainz".to_owned()],
            image_fetcher_order: vec!["Fanart".to_owned()],
            ..MetadataOptions::default()
        };
        assert_eq!(
            metadata_fetcher_rank(None, Some(&global), "MusicAlbum", "TheAudioDB"),
            0
        );
        assert_eq!(
            metadata_fetcher_rank(None, Some(&global), "MusicAlbum", "MusicBrainz"),
            1
        );
        assert_eq!(
            image_fetcher_rank(None, Some(&global), "MusicAlbum", "Fanart"),
            0
        );
        assert_eq!(
            image_fetcher_rank(None, Some(&global), "MusicAlbum", "TheAudioDB"),
            usize::MAX
        );
        let saved = LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("MusicAlbum".to_owned()),
                ..TypeOptions::default()
            }],
            ..LibraryOptions::default()
        };
        assert_eq!(
            metadata_fetcher_rank(Some(&saved), Some(&global), "MusicAlbum", "TheAudioDB"),
            usize::MAX,
            "a saved empty order ranks nothing"
        );
        assert_eq!(
            image_fetcher_rank(Some(&saved), Some(&global), "MusicAlbum", "Fanart"),
            usize::MAX
        );
        assert_eq!(
            metadata_fetcher_rank(None, None, "MusicAlbum", "TheAudioDB"),
            usize::MAX
        );
    }

    #[test]
    fn configured_order_is_exact_while_enable_lists_ignore_case() {
        let options = LibraryOptions {
            type_options: vec![TypeOptions {
                type_: Some("Movie".to_owned()),
                metadata_fetchers: vec!["themoviedb".to_owned()],
                metadata_fetcher_order: vec!["themoviedb".to_owned()],
                image_fetchers: vec!["themoviedb".to_owned()],
                image_fetcher_order: vec!["themoviedb".to_owned()],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(super::metadata_fetcher_enabled(
            Some(&options),
            None,
            "Movie",
            "TheMovieDb"
        ));
        assert!(super::image_fetcher_enabled(
            Some(&options),
            None,
            "Movie",
            "TheMovieDb"
        ));
        assert_eq!(
            super::metadata_fetcher_rank(Some(&options), None, "Movie", "TheMovieDb"),
            usize::MAX
        );
        assert_eq!(
            super::image_fetcher_rank(Some(&options), None, "Movie", "TheMovieDb"),
            usize::MAX
        );
        assert_eq!(
            super::configured_order(&["TheMovieDb".to_owned()], "TheMovieDb"),
            0
        );
    }

    /// The image fetchers' supported types per kind, as `GetSupportedImages`
    /// declares them (`TmdbSeriesImageProvider.cs:47-53`,
    /// `TmdbEpisodeImageProvider.cs:46-49`, `TvdbSeriesImageProvider.cs:
    /// 59-66`, `TvdbSeasonImageProvider.cs:59-64`).
    #[test]
    fn image_fetchers_name_the_types_they_supply() {
        use ferrofin_model::entities::ImageType::{Art, Backdrop, Banner, Logo, Primary, Thumb};
        assert_eq!(
            super::tmdb_images("Series"),
            [Primary, Backdrop, Logo, Thumb]
        );
        assert_eq!(super::tmdb_images("Episode"), [Primary]);
        assert_eq!(
            super::tvdb_images("Series"),
            [Primary, Banner, Backdrop, Logo, Art]
        );
        assert_eq!(super::tvdb_images("Season"), [Primary, Banner, Backdrop]);
        assert_eq!(super::tvdb_images("Episode"), [Primary]);
        assert!(super::tvdb_images("Movie").is_empty());
        assert!(
            super::fanart_images("Season").is_empty(),
            "not advertised until its season fetch is ported"
        );
    }
    #[test]
    fn current_web_receives_available_similarity_provider_choices() {
        let dto = library_options_info(&["Movie".into()], true, &[], &[]);
        let body = serde_json::to_value(dto).unwrap();
        let choices = body["TypeOptions"][0]["SimilarItemProviders"]
            .as_array()
            .expect("the current Web library editor needs these choices");
        assert!(
            choices
                .iter()
                .any(|p| p["Name"] == "Local Genre/Tag" && p["DefaultEnabled"] == true)
        );
        assert!(
            choices
                .iter()
                .any(|p| p["Name"] == "TheMovieDb" && p["DefaultEnabled"] == false)
        );
        assert!(!choices.iter().any(|p| p["Name"] == "ListenBrainz"));
    }
}
