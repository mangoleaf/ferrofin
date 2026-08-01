//! Port of `MediaBrowser.Model.LiveTv`.
//!
//! [`ChannelType`] and [`ProgramAudio`] are the canonical home for the enums
//! that [`crate::dto`] previously stubbed as forward references (they are
//! re-exported from there). [`ItemSortBy`] is defined here as a forward
//! reference: upstream it lives in the out-of-tree `Jellyfin.Data.Enums`, it is
//! referenced by [`LiveTvChannelQuery`], and it has no dedicated port unit.
//!
//! C# class inheritance (`TimerInfoDto : BaseTimerInfoDto`) is flattened: the
//! base fields are duplicated into each derived struct, since Rust has no struct
//! inheritance.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::dto::{BaseItemDto, DayOfWeek, NameIdPair, NameValuePair, SortOrder};
use crate::entities::ImageType;
use crate::querying::ItemFields;

/// The type of a live TV channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
pub enum ChannelType {
    /// The TV.
    #[serde(rename = "TV")]
    Tv,
    /// The radio.
    Radio,
}

/// The audio format of a live TV program.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ProgramAudio {
    /// Mono audio.
    Mono,
    /// Stereo audio.
    Stereo,
    /// Dolby audio.
    Dolby,
    /// Dolby Digital audio.
    DolbyDigital,
    /// THX audio.
    Thx,
    /// Dolby Atmos audio.
    Atmos,
}

/// The day pattern of a recurring timer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum DayPattern {
    /// Every day.
    #[default]
    Daily,
    /// Monday through Friday.
    Weekdays,
    /// Saturday and Sunday.
    Weekends,
}

/// The status of a recording.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum RecordingStatus {
    /// A new recording.
    #[default]
    New,
    /// The recording is in progress.
    InProgress,
    /// The recording is completed.
    Completed,
    /// The recording was cancelled.
    Cancelled,
    /// The recording conflicted but is OK.
    ConflictedOk,
    /// The recording conflicted and is not OK.
    ConflictedNotOk,
    /// The recording errored.
    Error,
}

/// The status of a live TV service.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum LiveTvServiceStatus {
    /// The service is available.
    #[default]
    Ok = 0,
    /// The service is unavailable.
    Unavailable = 1,
}

/// How long a recording should be kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum KeepUntil {
    /// Keep until deleted.
    #[default]
    UntilDeleted,
    /// Keep until space is needed.
    UntilSpaceNeeded,
    /// Keep until watched.
    UntilWatched,
    /// Keep until a date.
    UntilDate,
}

/// The sort field for items (mirrors `Jellyfin.Data.Enums.ItemSortBy`).
///
/// Forward reference: defined here because [`LiveTvChannelQuery`] uses it and it
/// has no dedicated port unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(missing_docs)]
pub enum ItemSortBy {
    /// The default sort order.
    #[default]
    Default,
    AiredEpisodeOrder,
    Album,
    AlbumArtist,
    Artist,
    DateCreated,
    OfficialRating,
    DatePlayed,
    PremiereDate,
    StartDate,
    SortName,
    Name,
    Random,
    Runtime,
    CommunityRating,
    ProductionYear,
    PlayCount,
    CriticRating,
    IsFolder,
    IsUnplayed,
    IsPlayed,
    SeriesSortName,
    #[serde(rename = "VideoBitRate")]
    VideoBitRate,
    AirTime,
    Studio,
    IsFavoriteOrLiked,
    DateLastContentAdded,
    SeriesDatePlayed,
    ParentIndexNumber,
    IndexNumber,
}

/// Guide date range info.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct GuideInfo {
    /// Gets or sets the start date.
    #[schema(value_type = String, format = "date-time")]
    pub start_date: DateTime<Utc>,

    /// Gets or sets the end date.
    #[schema(value_type = String, format = "date-time")]
    pub end_date: DateTime<Utc>,
}

/// Information about a live TV service.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LiveTvServiceInfo {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the home page URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub home_page_url: Option<String>,

    /// Gets or sets the status.
    pub status: LiveTvServiceStatus,

    /// Gets or sets the status message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status_message: Option<String>,

    /// Gets or sets the version.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Gets or sets a value indicating whether this instance has an update
    /// available.
    pub has_update_available: bool,

    /// Gets or sets a value indicating whether this instance is visible.
    pub is_visible: bool,

    /// Gets or sets the tuners.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tuners: Option<Vec<String>>,
}

/// Aggregate live TV info.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LiveTvInfo {
    /// Gets or sets the services.
    pub services: Vec<LiveTvServiceInfo>,

    /// Gets or sets a value indicating whether this instance is enabled.
    pub is_enabled: bool,

    /// Gets or sets the enabled users.
    pub enabled_users: Vec<String>,
}

/// A mapping between a tuner channel and a provider channel.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct TunerChannelMapping {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the provider channel name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_channel_name: Option<String>,

    /// Gets or sets the provider channel id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_channel_id: Option<String>,

    /// Gets or sets the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
}

/// Channel mapping options DTO.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ChannelMappingOptionsDto {
    /// Gets or sets the list of tuner channels.
    pub tuner_channels: Vec<TunerChannelMapping>,

    /// Gets or sets the list of provider channels.
    pub provider_channels: Vec<NameIdPair>,

    /// Gets or sets the list of mappings.
    pub mappings: Vec<NameValuePair>,

    /// Gets or sets the provider name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_name: Option<String>,
}

/// Base timer info DTO (flattened into [`TimerInfoDto`] and
/// [`SeriesTimerInfoDto`]).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
// `default`: flattened into TimerInfoDto/SeriesTimerInfoDto, so a client body omitting any base
// field would 422 the whole request (container default on the outer type can't cover a flattened
// inner's required fields). Mirrors TypeOptions; faithful to Jellyfin's System.Text.Json.
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)]
pub struct BaseTimerInfoDto {
    /// Gets or sets the id of the recording.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the type.
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,

    /// Gets or sets the server identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub server_id: Option<String>,

    /// Gets or sets the external identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,

    /// Gets or sets the channel id of the recording.
    #[schema(value_type = String, format = "uuid")]
    pub channel_id: Uuid,

    /// Gets or sets the external channel identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_channel_id: Option<String>,

    /// Gets or sets the channel name of the recording.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,

    /// Gets or sets the channel primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_primary_image_tag: Option<String>,

    /// Gets or sets the program identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_id: Option<String>,

    /// Gets or sets the external program identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_program_id: Option<String>,

    /// Gets or sets the name of the recording.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the description of the recording.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,

    /// Gets or sets the start date of the recording, in UTC.
    #[schema(value_type = String, format = "date-time")]
    pub start_date: DateTime<Utc>,

    /// Gets or sets the end date of the recording, in UTC.
    #[schema(value_type = String, format = "date-time")]
    pub end_date: DateTime<Utc>,

    /// Gets or sets the name of the service.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub service_name: Option<String>,

    /// Gets or sets the priority.
    pub priority: i32,

    /// Gets or sets the pre padding seconds.
    pub pre_padding_seconds: i32,

    /// Gets or sets the post padding seconds.
    pub post_padding_seconds: i32,

    /// Gets or sets a value indicating whether pre padding is required.
    pub is_pre_padding_required: bool,

    /// Gets or sets the id of the parent that has a backdrop.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_backdrop_item_id: Option<String>,

    /// Gets or sets the parent backdrop image tags.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_backdrop_image_tags: Option<Vec<String>>,

    /// Gets or sets a value indicating whether post padding is required.
    pub is_post_padding_required: bool,

    /// Gets or sets how long the recording is kept.
    pub keep_until: KeepUntil,
}

/// Timer info DTO. Flattens [`BaseTimerInfoDto`].
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)]
pub struct TimerInfoDto {
    /// Flattened base timer info.
    #[serde(flatten)]
    pub base: BaseTimerInfoDto,

    /// Gets or sets the status.
    pub status: RecordingStatus,

    /// Gets or sets the series timer identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_timer_id: Option<String>,

    /// Gets or sets the external series timer identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_series_timer_id: Option<String>,

    /// Gets or sets the run time ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<i64>,

    /// Gets or sets the program information.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub program_info: Option<Box<BaseItemDto>>,
}

/// Series timer info DTO. Flattens [`BaseTimerInfoDto`].
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)]
pub struct SeriesTimerInfoDto {
    /// Flattened base timer info.
    #[serde(flatten)]
    pub base: BaseTimerInfoDto,

    /// Gets or sets a value indicating whether to record at any time.
    pub record_any_time: bool,

    /// Gets or sets a value indicating whether to skip episodes in the library.
    pub skip_episodes_in_library: bool,

    /// Gets or sets a value indicating whether to record on any channel.
    pub record_any_channel: bool,

    /// Gets or sets how many recordings to keep.
    pub keep_up_to: i32,

    /// Gets or sets a value indicating whether to record new episodes only.
    pub record_new_only: bool,

    /// Gets or sets the days.
    pub days: Vec<DayOfWeek>,

    /// Gets or sets the day pattern.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub day_pattern: Option<DayPattern>,

    /// Gets or sets the image tags.
    pub image_tags: HashMap<ImageType, String>,

    /// Gets or sets the parent thumb item id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thumb_item_id: Option<String>,

    /// Gets or sets the parent thumb image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thumb_image_tag: Option<String>,

    /// Gets or sets the parent primary image item identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_primary_image_item_id: Option<String>,

    /// Gets or sets the parent primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_primary_image_tag: Option<String>,
}

/// A query for timers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct TimerQuery {
    /// Gets or sets the channel identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,

    /// Gets or sets the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the series timer identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_timer_id: Option<String>,

    /// Gets or sets a value indicating whether the timer is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_active: Option<bool>,

    /// Gets or sets a value indicating whether the timer is scheduled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_scheduled: Option<bool>,
}

/// A query for series timers.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SeriesTimerQuery {
    /// Gets or sets the sort by field (`SortName`, `Priority`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<String>,

    /// Gets or sets the sort order.
    pub sort_order: SortOrder,
}

/// A query for recordings.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct RecordingQuery {
    /// Gets or sets the channel identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_id: Option<String>,

    /// Gets or sets the user identifier.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,

    /// Gets or sets the identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the start index. Use for paging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_index: Option<i32>,

    /// Gets or sets the maximum number of items to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,

    /// Gets or sets the status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<RecordingStatus>,

    /// Gets or sets a value indicating whether the recording is in progress.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_in_progress: Option<bool>,

    /// Gets or sets the series timer identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_timer_id: Option<String>,

    /// Gets or sets the fields to return within the items.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<ItemFields>>,

    /// Gets or sets a value indicating whether images are enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_images: Option<bool>,

    /// Gets or sets a value indicating whether the recording is a library item.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_library_item: Option<bool>,

    /// Gets or sets a value indicating whether the recording is news.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_news: Option<bool>,

    /// Gets or sets a value indicating whether the recording is a movie.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_movie: Option<bool>,

    /// Gets or sets a value indicating whether the recording is a series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_series: Option<bool>,

    /// Gets or sets a value indicating whether the recording is for kids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_kids: Option<bool>,

    /// Gets or sets a value indicating whether the recording is sports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_sports: Option<bool>,

    /// Gets or sets the image type limit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_type_limit: Option<i32>,

    /// Gets or sets the enabled image types.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable_image_types: Option<Vec<ImageType>>,

    /// Gets or sets a value indicating whether to enable the total record
    /// count.
    pub enable_total_record_count: bool,
}

/// A query for live TV channels.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct LiveTvChannelQuery {
    /// Gets or sets the type of the channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_type: Option<ChannelType>,

    /// Gets or sets a value indicating whether this instance is favorite.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_favorite: Option<bool>,

    /// Gets or sets a value indicating whether this instance is liked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_liked: Option<bool>,

    /// Gets or sets a value indicating whether this instance is disliked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_disliked: Option<bool>,

    /// Gets or sets a value indicating whether to enable favorite sorting.
    pub enable_favorite_sorting: bool,

    /// Gets or sets the user identifier.
    #[schema(value_type = String, format = "uuid")]
    pub user_id: Uuid,

    /// Gets or sets the start index. Used for paging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_index: Option<i32>,

    /// Gets or sets the maximum number of items to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,

    /// Gets or sets a value indicating whether to add the current program.
    pub add_current_program: bool,

    /// Gets or sets a value indicating whether user data is enabled.
    pub enable_user_data: bool,

    /// Gets or sets a value indicating whether to return news.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_news: Option<bool>,

    /// Gets or sets a value indicating whether to return movies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_movie: Option<bool>,

    /// Gets or sets a value indicating whether this instance is kids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_kids: Option<bool>,

    /// Gets or sets a value indicating whether this instance is sports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_sports: Option<bool>,

    /// Gets or sets a value indicating whether this instance is a series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_series: Option<bool>,

    /// Gets or sets the sort fields.
    pub sort_by: Vec<ItemSortBy>,

    /// Gets or sets the sort order to return results with.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_order: Option<SortOrder>,
}

/// Information about a listings provider.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
pub struct ListingsProviderInfo {
    /// Gets or sets the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the type.
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,

    /// Gets or sets the username.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub username: Option<String>,

    /// Gets or sets the password.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,

    /// Gets or sets the listings id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listings_id: Option<String>,

    /// Gets or sets the zip code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zip_code: Option<String>,

    /// Gets or sets the country.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,

    /// Gets or sets the path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Gets or sets the enabled tuners.
    pub enabled_tuners: Vec<String>,

    /// Gets or sets a value indicating whether all tuners are enabled.
    pub enable_all_tuners: bool,

    /// Gets or sets the news categories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub news_categories: Option<Vec<String>>,

    /// Gets or sets the sports categories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sports_categories: Option<Vec<String>>,

    /// Gets or sets the kids categories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kids_categories: Option<Vec<String>>,

    /// Gets or sets the movie categories.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub movie_categories: Option<Vec<String>>,

    /// Gets or sets the channel mappings.
    pub channel_mappings: Vec<NameValuePair>,

    /// Gets or sets the movie prefix.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub movie_prefix: Option<String>,

    /// Gets or sets the preferred language.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_language: Option<String>,

    /// Gets or sets the user agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,
}

/// Information about a tuner host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)]
pub struct TunerHostInfo {
    /// Gets or sets the id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    /// Gets or sets the type.
    #[serde(rename = "Type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,

    /// Gets or sets the device id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,

    /// Gets or sets the friendly name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub friendly_name: Option<String>,

    /// Gets or sets a value indicating whether to import favorites only.
    pub import_favorites_only: bool,

    /// Gets or sets a value indicating whether hardware transcoding is allowed.
    #[serde(rename = "AllowHWTranscoding")]
    pub allow_hw_transcoding: bool,

    /// Gets or sets a value indicating whether the fMP4 transcoding container is
    /// allowed.
    #[serde(rename = "AllowFmp4TranscodingContainer")]
    pub allow_fmp4_transcoding_container: bool,

    /// Gets or sets a value indicating whether stream sharing is allowed.
    pub allow_stream_sharing: bool,

    /// Gets or sets the fallback max streaming bitrate.
    pub fallback_max_streaming_bitrate: i32,

    /// Gets or sets a value indicating whether stream looping is enabled.
    pub enable_stream_looping: bool,

    /// Gets or sets the source.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,

    /// Gets or sets the tuner count.
    pub tuner_count: i32,

    /// Gets or sets the user agent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user_agent: Option<String>,

    /// Gets or sets a value indicating whether to ignore DTS.
    pub ignore_dts: bool,

    /// Gets or sets a value indicating whether to read at native framerate.
    pub read_at_native_framerate: bool,
}

impl Default for TunerHostInfo {
    fn default() -> Self {
        Self {
            id: None,
            url: None,
            type_: None,
            device_id: None,
            friendly_name: None,
            import_favorites_only: false,
            allow_hw_transcoding: true,
            allow_fmp4_transcoding_container: false,
            allow_stream_sharing: true,
            fallback_max_streaming_bitrate: 30_000_000,
            enable_stream_looping: false,
            source: None,
            tuner_count: 0,
            user_agent: None,
            ignore_dts: true,
            read_at_native_framerate: false,
        }
    }
}

/// Live TV options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct LiveTvOptions {
    /// Gets or sets the number of guide days.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guide_days: Option<i32>,

    /// Gets or sets the recording path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_path: Option<String>,

    /// Gets or sets the movie recording path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub movie_recording_path: Option<String>,

    /// Gets or sets the series recording path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_recording_path: Option<String>,

    /// Gets or sets a value indicating whether recording subfolders are enabled.
    pub enable_recording_subfolders: bool,

    /// Gets or sets a value indicating whether original audio is kept with
    /// encoded recordings.
    pub enable_original_audio_with_encoded_recordings: bool,

    /// Gets or sets the tuner hosts.
    pub tuner_hosts: Vec<TunerHostInfo>,

    /// Gets or sets the listing providers.
    pub listing_providers: Vec<ListingsProviderInfo>,

    /// Gets or sets the pre padding seconds.
    pub pre_padding_seconds: i32,

    /// Gets or sets the post padding seconds.
    pub post_padding_seconds: i32,

    /// Gets or sets the media locations created.
    pub media_locations_created: Vec<String>,

    /// Gets or sets the recording post processor.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_post_processor: Option<String>,

    /// Gets or sets the recording post processor arguments.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_post_processor_arguments: Option<String>,

    /// Gets or sets a value indicating whether to save the recording NFO.
    #[serde(rename = "SaveRecordingNFO")]
    pub save_recording_nfo: bool,

    /// Gets or sets a value indicating whether to save recording images.
    pub save_recording_images: bool,
}

impl Default for LiveTvOptions {
    fn default() -> Self {
        Self {
            guide_days: None,
            recording_path: None,
            movie_recording_path: None,
            series_recording_path: None,
            enable_recording_subfolders: false,
            enable_original_audio_with_encoded_recordings: false,
            tuner_hosts: Vec::new(),
            listing_providers: Vec::new(),
            pre_padding_seconds: 0,
            post_padding_seconds: 0,
            media_locations_created: Vec::new(),
            recording_post_processor: None,
            recording_post_processor_arguments: Some("\"{path}\"".to_owned()),
            save_recording_nfo: true,
            save_recording_images: true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_type_uses_tv_alias() {
        assert_eq!(serde_json::to_string(&ChannelType::Tv).unwrap(), "\"TV\"");
        assert_eq!(
            serde_json::to_string(&ChannelType::Radio).unwrap(),
            "\"Radio\""
        );
        let back: ChannelType = serde_json::from_str("\"TV\"").unwrap();
        assert_eq!(back, ChannelType::Tv);
    }

    #[test]
    fn program_audio_round_trips() {
        for variant in [
            ProgramAudio::Mono,
            ProgramAudio::Stereo,
            ProgramAudio::Dolby,
            ProgramAudio::DolbyDigital,
            ProgramAudio::Thx,
            ProgramAudio::Atmos,
        ] {
            let back: ProgramAudio =
                serde_json::from_str(&serde_json::to_string(&variant).unwrap()).unwrap();
            assert_eq!(variant, back);
        }
        assert_eq!(
            serde_json::to_string(&ProgramAudio::DolbyDigital).unwrap(),
            "\"DolbyDigital\""
        );
    }

    #[test]
    fn enum_defaults() {
        assert_eq!(DayPattern::default(), DayPattern::Daily);
        assert_eq!(RecordingStatus::default(), RecordingStatus::New);
        assert_eq!(LiveTvServiceStatus::default(), LiveTvServiceStatus::Ok);
        assert_eq!(KeepUntil::default(), KeepUntil::UntilDeleted);
        assert_eq!(ItemSortBy::default(), ItemSortBy::Default);
    }

    #[test]
    fn item_sort_by_video_bit_rate_alias() {
        assert_eq!(
            serde_json::to_string(&ItemSortBy::VideoBitRate).unwrap(),
            "\"VideoBitRate\""
        );
    }

    #[test]
    fn tuner_host_info_default_matches_upstream() {
        let host = TunerHostInfo::default();
        assert!(host.allow_hw_transcoding);
        assert!(host.allow_stream_sharing);
        assert_eq!(host.fallback_max_streaming_bitrate, 30_000_000);
        assert!(host.ignore_dts);
        assert!(!host.read_at_native_framerate);
    }

    #[test]
    fn tuner_host_info_hw_transcoding_field_name() {
        let json = serde_json::to_value(TunerHostInfo::default()).unwrap();
        assert_eq!(json["AllowHWTranscoding"], true);
        assert_eq!(json["AllowFmp4TranscodingContainer"], false);
    }

    #[test]
    fn live_tv_options_default_and_round_trip() {
        let opts = LiveTvOptions::default();
        assert!(opts.save_recording_nfo);
        assert!(opts.save_recording_images);
        assert_eq!(
            opts.recording_post_processor_arguments.as_deref(),
            Some("\"{path}\"")
        );
        let json = serde_json::to_value(&opts).unwrap();
        assert_eq!(json["SaveRecordingNFO"], true);
        let back: LiveTvOptions = serde_json::from_value(json).unwrap();
        assert_eq!(opts, back);
    }

    #[test]
    fn guide_info_round_trips() {
        let value = GuideInfo::default();
        let back: GuideInfo =
            serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn service_and_aggregate_info_round_trip() {
        let value = LiveTvInfo {
            services: vec![LiveTvServiceInfo {
                name: Some("HDHomeRun".to_owned()),
                status: LiveTvServiceStatus::Ok,
                is_visible: true,
                ..LiveTvServiceInfo::default()
            }],
            is_enabled: true,
            enabled_users: vec!["user1".to_owned()],
        };
        let back: LiveTvInfo =
            serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn timer_info_dto_flattens_base() {
        let value = TimerInfoDto {
            base: BaseTimerInfoDto {
                id: Some("timer1".to_owned()),
                channel_id: Uuid::from_u128(3),
                priority: 5,
                ..BaseTimerInfoDto::default()
            },
            status: RecordingStatus::InProgress,
            run_time_ticks: Some(1_000),
            ..TimerInfoDto::default()
        };
        let json = serde_json::to_value(&value).unwrap();
        // Flattened base fields sit at the top level.
        assert_eq!(json["Id"], "timer1");
        assert_eq!(json["Priority"], 5);
        assert_eq!(json["Status"], "InProgress");
        let back: TimerInfoDto = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn series_timer_and_queries_round_trip() {
        let series = SeriesTimerInfoDto {
            base: BaseTimerInfoDto::default(),
            record_any_time: true,
            days: vec![],
            day_pattern: Some(DayPattern::Weekdays),
            ..SeriesTimerInfoDto::default()
        };
        let back: SeriesTimerInfoDto =
            serde_json::from_str(&serde_json::to_string(&series).unwrap()).unwrap();
        assert_eq!(series, back);

        for _ in 0..1 {
            let tq = TimerQuery {
                channel_id: Some("ch1".to_owned()),
                is_active: Some(true),
                ..TimerQuery::default()
            };
            let back: TimerQuery =
                serde_json::from_str(&serde_json::to_string(&tq).unwrap()).unwrap();
            assert_eq!(tq, back);
        }

        let stq = SeriesTimerQuery::default();
        let back: SeriesTimerQuery =
            serde_json::from_str(&serde_json::to_string(&stq).unwrap()).unwrap();
        assert_eq!(stq, back);
    }

    #[test]
    fn recording_and_channel_queries_round_trip() {
        let rq = RecordingQuery {
            user_id: Uuid::from_u128(2),
            status: Some(RecordingStatus::Completed),
            enable_total_record_count: true,
            ..RecordingQuery::default()
        };
        let back: RecordingQuery =
            serde_json::from_str(&serde_json::to_string(&rq).unwrap()).unwrap();
        assert_eq!(rq, back);

        let cq = LiveTvChannelQuery {
            channel_type: Some(ChannelType::Tv),
            user_id: Uuid::from_u128(4),
            sort_by: vec![ItemSortBy::SortName],
            sort_order: Some(SortOrder::Descending),
            ..LiveTvChannelQuery::default()
        };
        let back: LiveTvChannelQuery =
            serde_json::from_str(&serde_json::to_string(&cq).unwrap()).unwrap();
        assert_eq!(cq, back);
    }

    #[test]
    fn tuner_mapping_and_listing_provider_round_trip() {
        let mapping = ChannelMappingOptionsDto {
            tuner_channels: vec![TunerChannelMapping {
                name: Some("CH1".to_owned()),
                ..TunerChannelMapping::default()
            }],
            provider_name: Some("SchedulesDirect".to_owned()),
            ..ChannelMappingOptionsDto::default()
        };
        let back: ChannelMappingOptionsDto =
            serde_json::from_str(&serde_json::to_string(&mapping).unwrap()).unwrap();
        assert_eq!(mapping, back);

        let listings = ListingsProviderInfo {
            id: Some("l1".to_owned()),
            enabled_tuners: vec!["t1".to_owned()],
            enable_all_tuners: true,
            ..ListingsProviderInfo::default()
        };
        let back: ListingsProviderInfo =
            serde_json::from_str(&serde_json::to_string(&listings).unwrap()).unwrap();
        assert_eq!(listings, back);
    }
}
