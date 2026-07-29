//! Metadata providers for Hermit — port of `MediaBrowser.Providers`
//! (+ `XbmcMetadata` NFO, `LocalMetadata`).
//!
//! Ports the `ProviderManager` (implements the `hermit-traits` trait), the
//! provider framework, the ffprobe-backed media-info provider, and local NFO
//! metadata. The remote API plugins (TMDB/MusicBrainz/OMDB/AudioDb/ListenBrainz)
//! are feature-gated and deferred (enrichment; need keys; not First-Light).
//! Filled by the Wave 5 PortJob. See `brain/PLAN_HERMIT_PORT.md` + `brain/DEFERRED.md`.

pub mod container_types;
pub mod library_options;
pub mod local_images;
pub mod mediainfo;
pub mod omdb;
#[cfg(feature = "opensubtitles")]
pub mod opensubtitles;
pub mod provider_manager;
pub mod tmdb;
pub mod xbmc;

#[cfg(feature = "opensubtitles")]
pub use opensubtitles::{OpenSubtitlesConfig, OpenSubtitlesProvider};

pub use omdb::OmdbClient;
pub use tmdb::{
    RemoteImage, SeasonImages, SeriesMatch, TmdbClient, TmdbDetails, TmdbImage, TmdbKind,
    TmdbPerson, TmdbSearchHit, TmdbTrailer,
};

pub use container_types::{
    FileSystemMetadata, ItemInfo, LocalImageInfo, MetadataResult, NfoItem, PersonInfo,
    RefreshResult, add_person, set_provider_id,
};
pub use local_images::{
    CollectionFolderLocalImageProvider, DirectoryService, EpisodeLocalImageProvider,
    FsDirectoryService, ImageItem, ImageItemKind, InternalMetadataFolderImageProvider,
    LocalImageProvider,
};
pub use mediainfo::{FFProbeVideoInfo, VideoProbeInput};
pub use provider_manager::{LocalProviderManager, RemoteSearchProvider, TmdbSearchProvider};
pub use xbmc::saver::{save_episode, save_movie, save_season, save_series};
