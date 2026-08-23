//! `UserItemDataDto` and `UpdateUserItemDataDto` — port of the matching
//! types in `MediaBrowser.Model.Dto`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

/// Per-user playback/favorite state for an item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct UserItemDataDto {
    /// Gets or sets the rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,

    /// Gets or sets the played percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub played_percentage: Option<f64>,

    /// Gets or sets the unplayed item count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unplayed_item_count: Option<i32>,

    /// Gets or sets the playback position ticks.
    pub playback_position_ticks: i64,

    /// Gets or sets the play count.
    pub play_count: i32,

    /// Gets or sets a value indicating whether this instance is a favorite.
    pub is_favorite: bool,

    /// Gets or sets a value indicating whether the item is liked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likes: Option<bool>,

    /// Gets or sets the last played date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub last_played_date: Option<DateTime<Utc>>,

    /// Gets or sets a value indicating whether the item is played.
    pub played: bool,

    /// Gets or sets the key.
    pub key: String,

    /// Gets or sets the item identifier.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub item_id: Uuid,
}

/// The subset of [`UserItemDataDto`] used to update user item data via the API.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct UpdateUserItemDataDto {
    /// Gets or sets the rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<f64>,

    /// Gets or sets the played percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub played_percentage: Option<f64>,

    /// Gets or sets the unplayed item count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unplayed_item_count: Option<i32>,

    /// Gets or sets the playback position ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_position_ticks: Option<i64>,

    /// Gets or sets the play count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_count: Option<i32>,

    /// Gets or sets a value indicating whether this instance is a favorite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_favorite: Option<bool>,

    /// Gets or sets a value indicating whether the item is liked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likes: Option<bool>,

    /// Gets or sets the last played date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub last_played_date: Option<DateTime<Utc>>,

    /// Gets or sets a value indicating whether the item is played.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub played: Option<bool>,

    /// Gets or sets the key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,

    /// Gets or sets the item identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,
}
