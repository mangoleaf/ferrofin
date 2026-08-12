//! Session request DTOs — port of `BrowseRequest`, `PlayRequest`, and
//! `PlaystateRequest` from `MediaBrowser.Model.Session`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{PlayCommand, PlaystateCommand};
use crate::data::BaseItemKind;

/// A request to browse to a particular item on a client.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct BrowseRequest {
    /// Gets or sets the item type.
    pub item_type: BaseItemKind,

    /// Gets or sets the item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_id: Option<String>,

    /// Gets or sets the name of the item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_name: Option<String>,
}

/// A request to start playback on a session.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlayRequest {
    /// Gets or sets the item ids.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub item_ids: Vec<Uuid>,

    /// Gets or sets the start position ticks the first item should play at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_position_ticks: Option<i64>,

    /// Gets or sets the play command.
    pub play_command: PlayCommand,

    /// Gets or sets the controlling user identifier.
    #[schema(value_type = String, format = "uuid")]
    pub controlling_user_id: Uuid,

    /// Gets or sets the subtitle stream index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subtitle_stream_index: Option<i32>,

    /// Gets or sets the audio stream index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_stream_index: Option<i32>,

    /// Gets or sets the media source id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_source_id: Option<String>,

    /// Gets or sets the start index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_index: Option<i32>,
}

/// A request to change the playstate of a session.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PlaystateRequest {
    /// Gets or sets the playstate command.
    pub command: PlaystateCommand,

    /// Gets or sets the seek position in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seek_position_ticks: Option<i64>,

    /// Gets or sets the controlling user identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub controlling_user_id: Option<String>,
}
