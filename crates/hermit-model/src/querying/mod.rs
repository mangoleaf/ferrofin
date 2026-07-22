//! Port of `MediaBrowser.Model.Querying`.
//!
//! `LatestItemsQuery` and `NextUpQuery` are deferred: they carry a server-side
//! `User` entity (`Jellyfin.Database.Implementations.Entities.User`), which is
//! not part of this port unit. Port them alongside that entity.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

mod filters;
mod query_result;

pub use filters::{QueryFilters, QueryFiltersLegacy};
pub use query_result::{AllThemeMediaResult, QueryResult, ThemeMediaResult};

/// Used to control the data that gets attached to `DtoBaseItems`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ItemFields {
    /// The air time.
    AirTime,
    /// The can delete.
    CanDelete,
    /// The can download.
    CanDownload,
    /// The channel information.
    ChannelInfo,
    /// The chapters.
    Chapters,
    /// The trickplay manifest.
    Trickplay,
    /// The child count.
    ChildCount,
    /// The cumulative run time ticks.
    CumulativeRunTimeTicks,
    /// The custom rating.
    CustomRating,
    /// The date created of the item.
    DateCreated,
    /// The date last media added.
    DateLastMediaAdded,
    /// Item display preferences.
    DisplayPreferencesId,
    /// The etag.
    Etag,
    /// The external urls.
    ExternalUrls,
    /// Genres.
    Genres,
    /// The item counts.
    ItemCounts,
    /// The media source count.
    MediaSourceCount,
    /// The media versions.
    MediaSources,
    /// The original title.
    OriginalTitle,
    /// The item overview.
    Overview,
    /// The id of the item's parent.
    ParentId,
    /// The physical path of the item.
    Path,
    /// The list of people for the item.
    People,
    /// Value indicating whether playback access is granted.
    PlayAccess,
    /// The production locations.
    ProductionLocations,
    /// The ids from IMDb, TMDb, etc.
    ProviderIds,
    /// The aspect ratio of the primary image.
    PrimaryImageAspectRatio,
    /// The recursive item count.
    RecursiveItemCount,
    /// The settings.
    Settings,
    /// The series studio.
    SeriesStudio,
    /// The sort name of the item.
    SortName,
    /// The special episode numbers.
    SpecialEpisodeNumbers,
    /// The studios of the item.
    Studios,
    /// The taglines of the item.
    Taglines,
    /// The tags.
    Tags,
    /// The trailer url of the item.
    RemoteTrailers,
    /// The media streams.
    MediaStreams,
    /// The season user data.
    SeasonUserData,
    /// The last time metadata was refreshed.
    DateLastRefreshed,
    /// The last time metadata was saved.
    DateLastSaved,
    /// The refresh state.
    RefreshState,
    /// The channel image.
    ChannelImage,
    /// Value indicating whether media source display is enabled.
    EnableMediaSourceDisplay,
    /// The width.
    Width,
    /// The height.
    Height,
    /// The extra ids.
    ExtraIds,
    /// The local trailer count.
    LocalTrailerCount,
    /// Value indicating whether the item is HD.
    #[serde(rename = "IsHD")]
    IsHd,
    /// The special feature count.
    SpecialFeatureCount,
}

/// Enum `ItemFilter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ItemFilter {
    /// The item is a folder.
    IsFolder = 1,
    /// The item is not a folder.
    IsNotFolder = 2,
    /// The item is unplayed.
    IsUnplayed = 3,
    /// The item is played.
    IsPlayed = 4,
    /// The item is a favorite.
    IsFavorite = 5,
    /// The item is resumable.
    IsResumable = 7,
    /// The item is liked.
    Likes = 8,
    /// The item is disliked.
    Dislikes = 9,
    /// The item is a favorite or liked.
    IsFavoriteOrLikes = 10,
}
