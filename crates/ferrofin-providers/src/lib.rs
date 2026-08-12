//! Metadata providers for Ferrofin — port of `MediaBrowser.Providers`
//! (+ `XbmcMetadata` NFO, `LocalMetadata`).
//!
//! Ports the `ProviderManager` (implements the `ferrofin-traits` trait), the
//! provider framework, the ffprobe-backed media-info provider, and local NFO
//! metadata. The remote API plugins (TMDB/MusicBrainz/OMDB/AudioDb/ListenBrainz)
//! are feature-gated and deferred (enrichment; need keys; not First-Light).

pub mod audiodb;
pub mod container_types;
pub mod error;
pub mod fanart;
pub mod library_options;
pub mod local_images;
#[cfg(feature = "lrclib")]
pub mod lrclib;
pub mod mediainfo;
#[cfg(test)]
mod mock_http;
pub mod musicbrainz;
pub mod omdb;
#[cfg(feature = "opensubtitles")]
pub mod opensubtitles;
pub mod provider_manager;
pub mod studios;
pub mod tmdb;
pub mod tvdb;
pub mod xbmc;

pub use error::ProvidersError;

#[cfg(feature = "lrclib")]
pub use lrclib::{LrcLibConfig, LrcLibProvider};

#[cfg(feature = "opensubtitles")]
pub use opensubtitles::{OpenSubtitlesConfig, OpenSubtitlesProvider};

pub use audiodb::{AudioDbAlbum, AudioDbArtist, AudioDbClient};
pub use fanart::FanartClient;
pub use musicbrainz::{AlbumIds, MusicBrainzClient};
pub use omdb::OmdbClient;
pub use studios::StudiosClient;
pub use tmdb::{
    RemoteImage, SeasonImages, SeriesMatch, TmdbClient, TmdbDetails, TmdbImage, TmdbKind,
    TmdbPerson, TmdbSearchHit, TmdbTrailer,
};
pub use tvdb::{
    TvdbClient, TvdbEpisodeDetails, TvdbPerson, TvdbPersonDetails, TvdbSearchHit,
    TvdbSeasonDetails, TvdbSeriesDetails,
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
pub use provider_manager::{
    LocalProviderManager, RemoteSearchProvider, TmdbSearchProvider, TvdbSearchProvider,
};
pub use xbmc::saver::{save_episode, save_movie, save_season, save_series};
