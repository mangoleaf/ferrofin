//! Session playback DTOs — port of `PlaybackProgressInfo`,
//! `PlaybackStartInfo`, `PlaybackStopInfo`, `QueueItem`, `SessionUserInfo`, and
//! `UserDataChangeInfo` from `MediaBrowser.Model.Session`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{PlayMethod, PlaybackOrder, RepeatMode};
use crate::dto::{BaseItemDto, UserItemDataDto};

/// An item in a play queue.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct QueueItem {
    /// Gets or sets the item id.
    #[schema(value_type = String, format = "uuid")]
    pub id: Uuid,

    /// Gets or sets the playlist item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,
}

/// Class `PlaybackProgressInfo`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackProgressInfo {
    /// Gets or sets a value indicating whether this instance can seek.
    pub can_seek: bool,

    /// Gets or sets the item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<BaseItemDto>,

    /// Gets or sets the item identifier.
    #[schema(value_type = String, format = "uuid")]
    pub item_id: Uuid,

    /// Gets or sets the session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Gets or sets the media version identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,

    /// Gets or sets the index of the audio stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,

    /// Gets or sets the index of the subtitle stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,

    /// Gets or sets a value indicating whether this instance is paused.
    pub is_paused: bool,

    /// Gets or sets a value indicating whether this instance is muted.
    pub is_muted: bool,

    /// Gets or sets the position ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ticks: Option<i64>,

    /// Gets or sets the playback start time ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playback_start_time_ticks: Option<i64>,

    /// Gets or sets the volume level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_level: Option<i32>,

    /// Gets or sets the brightness.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<i32>,

    /// Gets or sets the aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub aspect_ratio: Option<String>,

    /// Gets or sets the play method.
    pub play_method: PlayMethod,

    /// Gets or sets the live stream identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<String>,

    /// Gets or sets the play session identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,

    /// Gets or sets the repeat mode.
    pub repeat_mode: RepeatMode,

    /// Gets or sets the playback order.
    pub playback_order: PlaybackOrder,

    /// Gets or sets the now-playing queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_playing_queue: Option<Vec<QueueItem>>,

    /// Gets or sets the playlist item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,
}

/// Class `PlaybackStartInfo`.
///
/// Upstream this derives from `PlaybackProgressInfo` with no additional
/// members; modeled here as a transparent alias.
pub type PlaybackStartInfo = PlaybackProgressInfo;

/// Class `PlaybackStopInfo`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaybackStopInfo {
    /// Gets or sets the item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item: Option<BaseItemDto>,

    /// Gets or sets the item identifier.
    #[schema(value_type = String, format = "uuid")]
    pub item_id: Uuid,

    /// Gets or sets the session id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,

    /// Gets or sets the media version identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,

    /// Gets or sets the position ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ticks: Option<i64>,

    /// Gets or sets the live stream identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<String>,

    /// Gets or sets the play session identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_session_id: Option<String>,

    /// Gets or sets a value indicating whether this info is failed.
    pub failed: bool,

    /// Gets or sets the next media type.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_media_type: Option<String>,

    /// Gets or sets the playlist item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,

    /// Gets or sets the now-playing queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_playing_queue: Option<Vec<QueueItem>>,
}

/// Class `SessionUserInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SessionUserInfo {
    /// Gets or sets the user identifier.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,

    /// Gets or sets the name of the user.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
}

/// Class `UserDataChangeInfo`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct UserDataChangeInfo {
    /// Gets or sets the user id.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,

    /// Gets or sets the user data list.
    pub user_data_list: Vec<UserItemDataDto>,
}
