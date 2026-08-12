//! `PlayerStateInfo` — port of `MediaBrowser.Model.Session.PlayerStateInfo`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use super::{PlayMethod, PlaybackOrder, RepeatMode};

/// The playback state of a session's now-playing item.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlayerStateInfo {
    /// Gets or sets the now-playing position ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub position_ticks: Option<i64>,

    /// Gets or sets a value indicating whether this instance can seek.
    pub can_seek: bool,

    /// Gets or sets a value indicating whether this instance is paused.
    pub is_paused: bool,

    /// Gets or sets a value indicating whether this instance is muted.
    pub is_muted: bool,

    /// Gets or sets the volume level.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume_level: Option<i32>,

    /// Gets or sets the index of the now-playing audio stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,

    /// Gets or sets the index of the now-playing subtitle stream.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,

    /// Gets or sets the now-playing media version identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,

    /// Gets or sets the play method.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_method: Option<PlayMethod>,

    /// Gets or sets the repeat mode.
    pub repeat_mode: RepeatMode,

    /// Gets or sets the playback order.
    pub playback_order: PlaybackOrder,

    /// Gets or sets the now-playing live stream identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub live_stream_id: Option<String>,
}
