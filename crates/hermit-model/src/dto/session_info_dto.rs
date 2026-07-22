//! `SessionInfoDto` — port of `MediaBrowser.Model.Dto.SessionInfoDto`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use super::{BaseItemDto, ClientCapabilitiesDto};
use crate::data::MediaType;
use crate::session::{
    GeneralCommandType, PlayerStateInfo, QueueItem, SessionUserInfo, TranscodingInfo,
};

/// Session info DTO.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct SessionInfoDto {
    /// Gets or sets the play state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub play_state: Option<PlayerStateInfo>,

    /// Gets or sets the additional users.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub additional_users: Option<Vec<SessionUserInfo>>,

    /// Gets or sets the client capabilities.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capabilities: Option<ClientCapabilitiesDto>,

    /// Gets or sets the remote end point.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_end_point: Option<String>,

    /// Gets or sets the playable media types.
    pub playable_media_types: Vec<MediaType>,

    /// Gets or sets the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the user id.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,

    /// Gets or sets the username.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,

    /// Gets or sets the type of the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,

    /// Gets or sets the last activity date.
    #[schema(value_type = String, format = "date-time")]
    pub last_activity_date: DateTime<Utc>,

    /// Gets or sets the last playback check-in.
    #[schema(value_type = String, format = "date-time")]
    pub last_playback_check_in: DateTime<Utc>,

    /// Gets or sets the last paused date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub last_paused_date: Option<DateTime<Utc>>,

    /// Gets or sets the name of the device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,

    /// Gets or sets the type of the device.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_type: Option<String>,

    /// Gets or sets the now playing item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_playing_item: Option<BaseItemDto>,

    /// Gets or sets the now viewing item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_viewing_item: Option<BaseItemDto>,

    /// Gets or sets the device id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,

    /// Gets or sets the application version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub application_version: Option<String>,

    /// Gets or sets the transcoding info.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transcoding_info: Option<TranscodingInfo>,

    /// Gets or sets a value indicating whether this session is active.
    pub is_active: bool,

    /// Gets or sets a value indicating whether the session supports media control.
    pub supports_media_control: bool,

    /// Gets or sets a value indicating whether the session supports remote control.
    pub supports_remote_control: bool,

    /// Gets or sets the now playing queue.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub now_playing_queue: Option<Vec<QueueItem>>,

    /// Gets or sets a value indicating whether this session has a custom device name.
    pub has_custom_device_name: bool,

    /// Gets or sets the playlist item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_item_id: Option<String>,

    /// Gets or sets the server id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,

    /// Gets or sets the user primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_primary_image_tag: Option<String>,

    /// Gets or sets the supported commands.
    pub supported_commands: Vec<GeneralCommandType>,
}
