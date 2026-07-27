//! Live TV manager trait.
//!
//! Port of the read/config slice of
//! `MediaBrowser.Controller.LiveTv.ILiveTvManager` plus the tuner-host and
//! listing-provider configuration surface and the DVR timer/series-timer/
//! recording CRUD.
//!
//! Port rules applied: DTO-shaped results reuse `hermit-model` DTOs
//! ([`LiveTvInfo`], [`TunerHostInfo`], [`ListingsProviderInfo`],
//! `QueryResult<BaseItemDto>`); identity args are [`uuid::Uuid`]; `Task<T>` →
//! `async fn -> Result<T, ServiceError>`.

use async_trait::async_trait;
use uuid::Uuid;

use hermit_model::dto::BaseItemDto;
use hermit_model::live_tv::{
    ListingsProviderInfo, LiveTvInfo, SeriesTimerInfoDto, TimerInfoDto, TunerHostInfo,
};
use hermit_model::querying::QueryResult;

use crate::error::ServiceError;
use crate::options::{DtoOptions, InternalItemsQuery};

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
    async fn get_channels(
        &self,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError>;

    /// Gets a single channel by id, or `None` when it is unknown.
    async fn get_channel(
        &self,
        id: Uuid,
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
    async fn get_program(
        &self,
        id: Uuid,
        options: &DtoOptions,
    ) -> Result<Option<BaseItemDto>, ServiceError>;

    /// Resets the tuner backing the given channel/recording id.
    async fn reset_tuner(&self, id: &str) -> Result<(), ServiceError>;

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

    /// Deletes a recording (its DB row and, when present, its file).
    async fn delete_recording(&self, id: Uuid) -> Result<(), ServiceError>;
}

fn _assert_object_safe_live_tv_manager(_: &dyn LiveTvManager) {}
