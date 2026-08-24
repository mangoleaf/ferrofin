//! Live TV manager trait.
//!
//! Port of the read/config slice of
//! `MediaBrowser.Controller.LiveTv.ILiveTvManager` plus the tuner-host and
//! listing-provider configuration surface and the DVR timer/series-timer/
//! recording CRUD.
//!
//! Port rules applied: DTO-shaped results reuse `ferrofin-model` DTOs
//! ([`LiveTvInfo`], [`TunerHostInfo`], [`ListingsProviderInfo`],
//! `QueryResult<BaseItemDto>`); identity args are [`uuid::Uuid`]; `Task<T>` →
//! `async fn -> Result<T, ServiceError>`.

use async_trait::async_trait;
use uuid::Uuid;

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::{BaseItemDto, SortOrder};
use ferrofin_model::live_tv::{
    ChannelType, ItemSortBy, ListingsProviderInfo, LiveTvInfo, SeriesTimerInfoDto, TimerInfoDto,
    TunerHostInfo,
};
use ferrofin_model::querying::QueryResult;

use crate::error::ServiceError;
use crate::options::{DtoOptions, InternalItemsQuery};

/// The channel-list query `GET /LiveTv/Channels` binds.
///
/// Port of `MediaBrowser.Model.LiveTv.LiveTvChannelQuery`, with the C# `UserId`
/// resolved to the requesting user's row (the crate-wide `User` → [`UserEntity`]
/// rule) — the user drives the favorite/like filters, favorite-first sorting and
/// the projected `UserData`.
#[derive(Debug, Clone, Default)]
#[allow(clippy::struct_excessive_bools)] // one field per upstream query property
pub struct LiveTvChannelQuery {
    /// Restrict to one channel type (TV or Radio).
    pub channel_type: Option<ChannelType>,
    /// The requesting user, if any.
    pub user: Option<UserEntity>,
    /// The index of the first record to return.
    pub start_index: Option<i32>,
    /// The maximum number of records to return.
    pub limit: Option<i32>,
    /// Restrict to channels the user has (not) favourited.
    pub is_favorite: Option<bool>,
    /// Restrict to channels the user has (not) liked (a rating at or above
    /// upstream's `UserItemData.MinLikeValue` of 6.5).
    pub is_liked: Option<bool>,
    /// Restrict to channels the user has (not) disliked. Accepted but never
    /// applied — upstream's `GetInternalChannels` drops it on the floor too.
    pub is_disliked: Option<bool>,
    /// Whether favourited/liked channels sort first.
    pub enable_favorite_sorting: bool,
    /// Restrict to movie channels.
    pub is_movie: Option<bool>,
    /// Restrict to series channels.
    pub is_series: Option<bool>,
    /// Restrict to news channels.
    pub is_news: Option<bool>,
    /// Restrict to kids' channels.
    pub is_kids: Option<bool>,
    /// Restrict to sports channels.
    pub is_sports: Option<bool>,
    /// The requested sort columns, in order.
    pub sort_by: Vec<ItemSortBy>,
    /// The sort order applied to every [`Self::sort_by`] column.
    pub sort_order: Option<SortOrder>,
    /// Whether each channel DTO carries its currently-airing programme.
    pub add_current_program: bool,
}

/// The Live TV manager.
///
/// Port of `ILiveTvManager` (read + configuration slice).
#[async_trait]
pub trait LiveTvManager: Send + Sync {
    /// Gets top-level Live TV service/status information.
    async fn get_live_tv_info(&self) -> Result<LiveTvInfo, ServiceError>;

    /// Lists the configured M3U tuner hosts.
    async fn get_tuner_hosts(&self) -> Result<Vec<TunerHostInfo>, ServiceError>;

    /// Saves (adds or updates) a tuner host, returning the stored value with its
    /// assigned id.
    async fn save_tuner_host(&self, info: TunerHostInfo) -> Result<TunerHostInfo, ServiceError>;

    /// Deletes the tuner host with the given id (and its cached channels).
    async fn delete_tuner_host(&self, id: &str) -> Result<(), ServiceError>;

    /// Lists the configured XMLTV listing providers.
    async fn get_listing_providers(&self) -> Result<Vec<ListingsProviderInfo>, ServiceError>;

    /// Saves (adds or updates) a listing provider, returning the stored value
    /// with its assigned id.
    async fn save_listing_provider(
        &self,
        info: ListingsProviderInfo,
    ) -> Result<ListingsProviderInfo, ServiceError>;

    /// Deletes the listing provider with the given id.
    async fn delete_listing_provider(&self, id: &str) -> Result<(), ServiceError>;

    /// Queries Live TV channels as `BaseItemDto`s (`Type = "TvChannel"`).
    ///
    /// Port of `GetInternalChannels` + the controller's projection: the query's
    /// filters/sort/paging apply, the DTOs project through the DTO service with
    /// the list-path `RemoveFields` strip, and each carries its channel info
    /// (and current programme when `query.add_current_program`).
    async fn get_channels(
        &self,
        query: &LiveTvChannelQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError>;

    /// Gets a single channel by id, or `None` when it is unknown.
    ///
    /// Port of `LiveTvController.GetChannel`'s projection: all requested fields
    /// survive (no list-path strip), with `user` driving `UserData`.
    async fn get_channel(
        &self,
        id: Uuid,
        user: Option<&UserEntity>,
        options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError>;

    /// Queries Live TV programs (EPG entries) as `BaseItemDto`s
    /// (`Type = "LiveTvProgram"`).
    async fn get_programs(
        &self,
        query: &InternalItemsQuery,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError>;

    /// Gets a single program by id, or `None` when it is unknown.
    ///
    /// Port of `LiveTvManager.GetProgram(id, ct, user)`. The contract:
    /// implementations project the full requested field set and apply the
    /// programme/recording post-passes, with `user` driving `UserData`.
    async fn get_program(
        &self,
        id: Uuid,
        user: Option<&UserEntity>,
        options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError>;

    /// Resets the tuner backing the given channel/recording id.
    async fn reset_tuner(&self, id: &str) -> Result<(), ServiceError>;

    /// Whether any tuner host is configured — the synchronous fact the
    /// "Refresh Guide" task's hidden rule reads (C# `IsHidden =>
    /// Services.Count == 1 && TunerHosts.Length == 0`, and a stock server has
    /// exactly one service). Defaults to `false`; the real manager maintains
    /// a flag on tuner-host save/delete and seeds it on the first read.
    fn has_tuner_hosts(&self) -> bool {
        false
    }

    /// Refreshes the channel lineup and guide by fetching every configured
    /// tuner host (M3U) and listing provider (XMLTV) and rewriting the cache.
    async fn refresh_guide(&self) -> Result<(), ServiceError>;

    /// Resolves a channel id to the tuner stream URL that plays it, or `None`
    /// when the channel is unknown.
    async fn get_channel_stream_url(&self, id: Uuid) -> Result<Option<String>, ServiceError>;

    // ---- DVR: recording timers -------------------------------------------

    /// Lists the scheduled recording timers.
    async fn get_timers(&self) -> Result<Vec<TimerInfoDto>, ServiceError>;

    /// Gets a single timer by id, or `None` when unknown.
    async fn get_timer(&self, id: &str) -> Result<Option<TimerInfoDto>, ServiceError>;

    /// Creates (or replaces) a recording timer, returning its id.
    async fn create_timer(&self, timer: TimerInfoDto) -> Result<String, ServiceError>;

    /// Updates the timer with the given id.
    async fn update_timer(&self, id: &str, timer: TimerInfoDto) -> Result<(), ServiceError>;

    /// Cancels (deletes) the timer with the given id.
    async fn cancel_timer(&self, id: &str) -> Result<(), ServiceError>;

    // ---- DVR: series timers ----------------------------------------------

    /// Lists the recurring (series) recording timers.
    async fn get_series_timers(&self) -> Result<Vec<SeriesTimerInfoDto>, ServiceError>;

    /// Gets a single series timer by id, or `None` when unknown.
    async fn get_series_timer(&self, id: &str) -> Result<Option<SeriesTimerInfoDto>, ServiceError>;

    /// Creates (or replaces) a series timer, returning its id.
    async fn create_series_timer(&self, timer: SeriesTimerInfoDto) -> Result<String, ServiceError>;

    /// Updates the series timer with the given id.
    async fn update_series_timer(
        &self,
        id: &str,
        timer: SeriesTimerInfoDto,
    ) -> Result<(), ServiceError>;

    /// Cancels (deletes) the series timer and its pending timers.
    async fn cancel_series_timer(&self, id: &str) -> Result<(), ServiceError>;

    // ---- DVR: recordings -------------------------------------------------

    /// Lists recordings as `BaseItemDto`s (`Type = "Recording"`).
    async fn get_recordings(&self) -> Result<QueryResult<BaseItemDto>, ServiceError>;

    /// Gets a single recording by id, or `None` when unknown.
    async fn get_recording(&self, id: Uuid) -> Result<Option<BaseItemDto>, ServiceError>;

    /// The on-disk path of a recording's captured file, or `None` when the
    /// recording is unknown or has no file yet. Backs
    /// `GET /LiveTv/LiveRecordings/{recordingId}/stream`.
    async fn get_recording_path(&self, id: Uuid) -> Result<Option<String>, ServiceError>;

    /// Deletes a recording (its DB row and, when present, its file).
    async fn delete_recording(&self, id: Uuid) -> Result<(), ServiceError>;
}

fn _assert_object_safe_live_tv_manager(_: &dyn LiveTvManager) {}
