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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[repr(i32)]
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
    /// An unnamed C# enum value, retained on read and written as a number.
    #[serde(untagged)]
    Unrecognized(i32),
}

crate::json::enums::wire_enum! {
    ItemFields, None, {
        AirTime => ("AirTime", 0),
        CanDelete => ("CanDelete", 1),
        CanDownload => ("CanDownload", 2),
        ChannelInfo => ("ChannelInfo", 3),
        Chapters => ("Chapters", 4),
        Trickplay => ("Trickplay", 5),
        ChildCount => ("ChildCount", 6),
        CumulativeRunTimeTicks => ("CumulativeRunTimeTicks", 7),
        CustomRating => ("CustomRating", 8),
        DateCreated => ("DateCreated", 9),
        DateLastMediaAdded => ("DateLastMediaAdded", 10),
        DisplayPreferencesId => ("DisplayPreferencesId", 11),
        Etag => ("Etag", 12),
        ExternalUrls => ("ExternalUrls", 13),
        Genres => ("Genres", 14),
        ItemCounts => ("ItemCounts", 15),
        MediaSourceCount => ("MediaSourceCount", 16),
        MediaSources => ("MediaSources", 17),
        OriginalTitle => ("OriginalTitle", 18),
        Overview => ("Overview", 19),
        ParentId => ("ParentId", 20),
        Path => ("Path", 21),
        People => ("People", 22),
        PlayAccess => ("PlayAccess", 23),
        ProductionLocations => ("ProductionLocations", 24),
        ProviderIds => ("ProviderIds", 25),
        PrimaryImageAspectRatio => ("PrimaryImageAspectRatio", 26),
        RecursiveItemCount => ("RecursiveItemCount", 27),
        Settings => ("Settings", 28),
        SeriesStudio => ("SeriesStudio", 29),
        SortName => ("SortName", 30),
        SpecialEpisodeNumbers => ("SpecialEpisodeNumbers", 31),
        Studios => ("Studios", 32),
        Taglines => ("Taglines", 33),
        Tags => ("Tags", 34),
        RemoteTrailers => ("RemoteTrailers", 35),
        MediaStreams => ("MediaStreams", 36),
        SeasonUserData => ("SeasonUserData", 37),
        DateLastRefreshed => ("DateLastRefreshed", 38),
        DateLastSaved => ("DateLastSaved", 39),
        RefreshState => ("RefreshState", 40),
        ChannelImage => ("ChannelImage", 41),
        EnableMediaSourceDisplay => ("EnableMediaSourceDisplay", 42),
        Width => ("Width", 43),
        Height => ("Height", 44),
        ExtraIds => ("ExtraIds", 45),
        LocalTrailerCount => ("LocalTrailerCount", 46),
        IsHd => ("IsHD", 47),
        SpecialFeatureCount => ("SpecialFeatureCount", 48),
    }
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
