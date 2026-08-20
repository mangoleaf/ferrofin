//! Metadata providers for Ferrofin — port of `MediaBrowser.Providers`
//! (+ `XbmcMetadata` NFO, `LocalMetadata`).
//!
//! Ports the `ProviderManager` (implements the `ferrofin-traits` trait), the
//! provider framework, the ffprobe-backed media-info provider, and local NFO
//! metadata. The remote providers (TMDB/TVDB/OMDb/fanart/MusicBrainz/AudioDb/
//! Studio Images) are compiled in unconditionally and gated at runtime by the
//! per-library fetcher checkboxes — OMDb additionally needs an API key before
//! it does anything.

pub mod audiodb;
pub mod books;
pub mod container_types;
pub mod error;
pub mod external_ids;
pub mod fanart;
pub mod library_options;
pub mod listenbrainz;
pub mod local_images;
pub mod local_xml;
#[cfg(feature = "lrclib")]
pub mod lrclib;
pub mod mediainfo;
#[cfg(test)]
mod mock_http;
pub mod musicbrainz;
pub mod omdb;
#[cfg(feature = "opensubtitles")]
pub mod opensubtitles;
pub mod playlist_file;
pub mod provider_manager;
pub mod similarity;
pub mod studios;
pub mod tmdb;
pub mod tvdb;
pub mod xbmc;

pub use books::{BookMetadata, read_book_cover, read_book_metadata};
pub use error::ProvidersError;
pub use external_ids::{ExternalIdItem, external_id_infos, external_urls};
pub use local_xml::{
    LocalContainerXml, LocalLinkedChild, parse_container_xml, save_collection_xml,
    save_playlist_xml,
};
pub use playlist_file::{is_playlist_file, read_playlist_file, write_playlist_file};

#[cfg(feature = "lrclib")]
pub use lrclib::{LrcLibConfig, LrcLibProvider};

#[cfg(feature = "opensubtitles")]
pub use opensubtitles::{OpenSubtitlesConfig, OpenSubtitlesProvider};

pub use audiodb::{AudioDbAlbum, AudioDbArtist, AudioDbClient};
pub use fanart::FanartClient;
pub use listenbrainz::{ListenBrainzClient, ListenBrainzConfig, SimilarityAlgorithm};
pub use musicbrainz::{AlbumIds, ArtistDetails, MusicBrainzClient, PartialDate, ReleaseDetails};
pub use omdb::{OmdbClient, OmdbItem, OmdbKind, OmdbPersonKind, OmdbSearchHit, OmdbSearchKey};
pub use similarity::{
    ListenBrainzSimilarArtistProvider, TMDB_SIMILAR_CACHE_DAYS, TmdbSimilarProvider,
};
pub use studios::StudiosClient;
pub use tmdb::{
    RemoteImage, SeasonImages, SeriesMatch, TmdbClient, TmdbCollection, TmdbCollectionHit,
    TmdbDetails, TmdbImage, TmdbKind, TmdbPerson, TmdbSearchHit, TmdbTrailer,
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
    LocalProviderManager, OmdbSearchProvider, RemoteSearchProvider, TmdbBoxSetSearchProvider,
    TmdbSearchProvider, TvdbSearchProvider,
};
pub use xbmc::saver::{
    NfoAlbum, NfoTrack, save_album, save_artist, save_episode, save_movie, save_season, save_series,
};
