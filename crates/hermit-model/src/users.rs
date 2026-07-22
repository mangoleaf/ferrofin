//! Port of `MediaBrowser.Model.Users`.
//!
//! [`SyncPlayUserAccessType`], [`AccessSchedule`] and [`DynamicDayOfWeek`] live
//! in the out-of-tree `Jellyfin.Database.Implementations` upstream; they are
//! defined here (as forward references) because [`UserPolicy`] embeds them in
//! the wire contract and they have no dedicated port unit.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::data::UnratedItem;

/// The action a user should take when they forgot their password.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ForgotPasswordAction {
    /// Contact the administrator.
    #[default]
    ContactAdmin = 0,
    /// Enter a pin code.
    PinCode = 1,
    /// The request must be made from within the network.
    InNetworkRequired = 2,
}

/// Access level to `SyncPlay` features (mirrors
/// `Jellyfin.Database.Implementations.Enums.SyncPlayUserAccessType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum SyncPlayUserAccessType {
    /// The user can create and join groups.
    #[default]
    CreateAndJoinGroups = 0,
    /// The user can only join existing groups.
    JoinGroups = 1,
    /// The user has no access to `SyncPlay`.
    None = 2,
}

/// The day of the week, or a group of days, an access schedule applies to
/// (mirrors `Jellyfin.Data.Enums.DynamicDayOfWeek`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum DynamicDayOfWeek {
    /// Sunday.
    #[default]
    Sunday,
    /// Monday.
    Monday,
    /// Tuesday.
    Tuesday,
    /// Wednesday.
    Wednesday,
    /// Thursday.
    Thursday,
    /// Friday.
    Friday,
    /// Saturday.
    Saturday,
    /// Every day.
    Everyday,
    /// Any weekday.
    Weekday,
    /// Any weekend day.
    Weekend,
}

/// An access schedule constraining when a user may access the server.
///
/// Forward reference: upstream this is
/// `Jellyfin.Database.Implementations.Entities.AccessSchedule`. Embedded in
/// [`UserPolicy`]; move when that entity is ported.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::derive_partial_eq_without_eq)]
pub struct AccessSchedule {
    /// Gets the id of this instance.
    pub id: i32,

    /// Gets the id of the associated user.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,

    /// Gets or sets the day of week.
    pub day_of_week: DynamicDayOfWeek,

    /// Gets or sets the start hour.
    pub start_hour: f64,

    /// Gets or sets the end hour.
    pub end_hour: f64,
}

/// The result of a forgot-password request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ForgotPasswordResult {
    /// Gets or sets the action.
    pub action: ForgotPasswordAction,

    /// Gets or sets the pin file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pin_file: Option<String>,

    /// Gets or sets the pin expiration date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    pub pin_expiration_date: Option<chrono::DateTime<chrono::Utc>>,
}

/// The result of redeeming a pin.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct PinRedeemResult {
    /// Gets or sets a value indicating whether this result is a success.
    pub success: bool,

    /// Gets or sets the users reset.
    pub users_reset: Vec<String>,
}

/// A user's access policy.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct UserPolicy {
    /// Gets or sets a value indicating whether this instance is administrator.
    pub is_administrator: bool,

    /// Gets or sets a value indicating whether this instance is hidden.
    pub is_hidden: bool,

    /// Gets or sets a value indicating whether this instance can manage
    /// collections.
    pub enable_collection_management: bool,

    /// Gets or sets a value indicating whether this instance can manage
    /// subtitles.
    pub enable_subtitle_management: bool,

    /// Gets or sets a value indicating whether this user can manage lyrics.
    pub enable_lyric_management: bool,

    /// Gets or sets a value indicating whether this instance is disabled.
    pub is_disabled: bool,

    /// Gets or sets the max parental rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_parental_rating: Option<i32>,

    /// Gets or sets the max parental sub rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_parental_sub_rating: Option<i32>,

    /// Gets or sets the blocked tags.
    pub blocked_tags: Vec<String>,

    /// Gets or sets the allowed tags.
    pub allowed_tags: Vec<String>,

    /// Gets or sets a value indicating whether user preference access is
    /// enabled.
    pub enable_user_preference_access: bool,

    /// Gets or sets the access schedules.
    pub access_schedules: Vec<AccessSchedule>,

    /// Gets or sets the unrated items that are blocked.
    pub block_unrated_items: Vec<UnratedItem>,

    /// Gets or sets a value indicating whether remote control of other users is
    /// enabled.
    pub enable_remote_control_of_other_users: bool,

    /// Gets or sets a value indicating whether shared device control is
    /// enabled.
    pub enable_shared_device_control: bool,

    /// Gets or sets a value indicating whether remote access is enabled.
    pub enable_remote_access: bool,

    /// Gets or sets a value indicating whether live TV management is enabled.
    pub enable_live_tv_management: bool,

    /// Gets or sets a value indicating whether live TV access is enabled.
    pub enable_live_tv_access: bool,

    /// Gets or sets a value indicating whether media playback is enabled.
    pub enable_media_playback: bool,

    /// Gets or sets a value indicating whether audio playback transcoding is
    /// enabled.
    pub enable_audio_playback_transcoding: bool,

    /// Gets or sets a value indicating whether video playback transcoding is
    /// enabled.
    pub enable_video_playback_transcoding: bool,

    /// Gets or sets a value indicating whether playback remuxing is enabled.
    pub enable_playback_remuxing: bool,

    /// Gets or sets a value indicating whether remote source transcoding is
    /// forced.
    pub force_remote_source_transcoding: bool,

    /// Gets or sets a value indicating whether content deletion is enabled.
    pub enable_content_deletion: bool,

    /// Gets or sets the folders content may be deleted from.
    pub enable_content_deletion_from_folders: Vec<String>,

    /// Gets or sets a value indicating whether content downloading is enabled.
    pub enable_content_downloading: bool,

    /// Gets or sets a value indicating whether sync transcoding is enabled.
    pub enable_sync_transcoding: bool,

    /// Gets or sets a value indicating whether media conversion is enabled.
    pub enable_media_conversion: bool,

    /// Gets or sets the enabled devices.
    pub enabled_devices: Vec<String>,

    /// Gets or sets a value indicating whether all devices are enabled.
    pub enable_all_devices: bool,

    /// Gets or sets the enabled channels.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub enabled_channels: Vec<Uuid>,

    /// Gets or sets a value indicating whether all channels are enabled.
    pub enable_all_channels: bool,

    /// Gets or sets the enabled folders.
    #[schema(value_type = Vec<String>, format = "uuid")]
    pub enabled_folders: Vec<Uuid>,

    /// Gets or sets a value indicating whether all folders are enabled.
    pub enable_all_folders: bool,

    /// Gets or sets the invalid login attempt count.
    pub invalid_login_attempt_count: i32,

    /// Gets or sets the number of login attempts before lockout.
    pub login_attempts_before_lockout: i32,

    /// Gets or sets the maximum number of active sessions.
    pub max_active_sessions: i32,

    /// Gets or sets a value indicating whether public sharing is enabled.
    pub enable_public_sharing: bool,

    /// Gets or sets the blocked media folders.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<String>>, format = "uuid")]
    pub blocked_media_folders: Option<Vec<Uuid>>,

    /// Gets or sets the blocked channels.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<Vec<String>>, format = "uuid")]
    pub blocked_channels: Option<Vec<Uuid>>,

    /// Gets or sets the remote client bitrate limit.
    pub remote_client_bitrate_limit: i32,

    /// Gets or sets the authentication provider id.
    pub authentication_provider_id: String,

    /// Gets or sets the password reset provider id.
    pub password_reset_provider_id: String,

    /// Gets or sets a value indicating what `SyncPlay` features the user can
    /// access.
    pub sync_play_access: SyncPlayUserAccessType,
}
