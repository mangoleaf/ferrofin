//! Port of `MediaBrowser.Model.Channels`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::entities::ImageType;
use crate::querying::ItemFields;

/// The folder type of a channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ChannelFolderType {
    /// A generic container.
    Container = 0,
    /// A music album.
    MusicAlbum = 1,
    /// A photo album.
    PhotoAlbum = 2,
    /// A music artist.
    MusicArtist = 3,
    /// A series.
    Series = 4,
    /// A season.
    Season = 5,
}

/// The sort field for channel items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ChannelItemSortField {
    /// Sort by name.
    Name = 0,
    /// Sort by community rating.
    CommunityRating = 1,
    /// Sort by premiere date.
    PremiereDate = 2,
    /// Sort by date created.
    DateCreated = 3,
    /// Sort by runtime.
    Runtime = 4,
    /// Sort by play count.
    PlayCount = 5,
    /// Sort by community play count.
    CommunityPlayCount = 6,
}

/// The content type of a channel media item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ChannelMediaContentType {
    /// A clip.
    Clip = 0,
    /// A podcast.
    Podcast = 1,
    /// A trailer.
    Trailer = 2,
    /// A movie.
    Movie = 3,
    /// An episode.
    Episode = 4,
    /// A song.
    Song = 5,
    /// A movie extra.
    MovieExtra = 6,
    /// A TV extra.
    TvExtra = 7,
}

/// The media type of a channel item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ChannelMediaType {
    /// Audio.
    Audio = 0,
    /// Video.
    Video = 1,
    /// Photo.
    Photo = 2,
}

/// The features supported by a channel.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct ChannelFeatures {
    /// Gets or sets the name.
    pub name: String,

    /// Gets or sets the identifier.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets a value indicating whether this instance can search.
    pub can_search: bool,

    /// Gets or sets the media types.
    pub media_types: Vec<ChannelMediaType>,

    /// Gets or sets the content types.
    pub content_types: Vec<ChannelMediaContentType>,

    /// Gets or sets the maximum number of records the channel allows retrieving
    /// at a time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_page_size: Option<i32>,

    /// Gets or sets the automatic refresh levels.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_refresh_levels: Option<i32>,

    /// Gets or sets the default sort orders.
    pub default_sort_fields: Vec<ChannelItemSortField>,

    /// Gets or sets a value indicating whether a sort ascending/descending
    /// toggle is supported.
    pub supports_sort_order_toggle: bool,

    /// Gets or sets a value indicating whether latest media is supported.
    pub supports_latest_media: bool,

    /// Gets or sets a value indicating whether this instance can filter.
    pub can_filter: bool,

    /// Gets or sets a value indicating whether content downloading is
    /// supported.
    pub supports_content_downloading: bool,
}

/// A query for channels.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ChannelQuery {
    /// Gets or sets the fields to return within the items, in addition to basic
    /// information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<ItemFields>>,

    /// Gets or sets a value indicating whether images are enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_images: Option<bool>,

    /// Gets or sets the image type limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_type_limit: Option<i32>,

    /// Gets or sets the enabled image types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_image_types: Option<Vec<ImageType>>,

    /// Gets or sets the user identifier.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub user_id: Uuid,

    /// Gets or sets the start index. Use for paging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_index: Option<i32>,

    /// Gets or sets the maximum number of items to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,

    /// Gets or sets a value indicating whether latest items are supported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_latest_items: Option<bool>,

    /// Gets or sets a value indicating whether media deletion is supported.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub supports_media_deletion: Option<bool>,

    /// Gets or sets a value indicating whether this instance is favorite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_favorite: Option<bool>,

    /// Gets or sets a value indicating whether this is the recordings folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_recordings_folder: Option<bool>,

    /// Gets or sets a value indicating whether to refresh latest channel items.
    pub refresh_latest_channel_items: bool,
}
