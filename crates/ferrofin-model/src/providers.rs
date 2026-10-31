//! Port of the portable DTOs in `MediaBrowser.Model.Providers`.
//!
//! Serde casing matches the Jellyfin JSON contract (PascalCase properties).
//! `RemoteLyricInfo` is deferred — it depends on `MediaBrowser.Model.Lyrics`
//! (`LyricMetadata`/`LyricResponse`), a namespace not yet ported.

mod external_id_info;
mod external_url;
mod image_provider_info;
mod lookup_info;
mod lyric_provider_info;
mod remote_image_info;
mod remote_image_query;
mod remote_image_result;
mod remote_search_result;
mod remote_subtitle_info;
mod subtitle_provider_info;

pub use external_id_info::{ExternalIdInfo, ExternalIdMediaType};
pub use external_url::ExternalUrl;
pub use image_provider_info::ImageProviderInfo;
pub use lookup_info::{
    AlbumInfo, ArtistInfo, BookInfo, BoxSetInfo, ItemLookupInfo, MovieInfo, MusicVideoInfo,
    PersonLookupInfo, RemoteSearchQuery, SeriesInfo, SongInfo, TrailerInfo,
};
pub use lyric_provider_info::LyricProviderInfo;
pub use remote_image_info::RemoteImageInfo;
pub use remote_image_query::RemoteImageQuery;
pub use remote_image_result::RemoteImageResult;
pub use remote_search_result::RemoteSearchResult;
pub use remote_subtitle_info::RemoteSubtitleInfo;
pub use subtitle_provider_info::SubtitleProviderInfo;
