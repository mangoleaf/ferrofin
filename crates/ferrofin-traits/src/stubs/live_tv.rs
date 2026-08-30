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
use chrono::{DateTime, Utc};
use uuid::Uuid;

use ferrofin_db::entities::users::UserEntity;
use ferrofin_model::dto::DayOfWeek;
use ferrofin_model::dto::{BaseItemDto, MediaSourceInfo, SortOrder};
use ferrofin_model::live_tv::{
    ChannelType, DayPattern, GuideInfo, ItemSortBy, KeepUntil, ListingsProviderInfo, LiveTvInfo,
    RecordingQuery, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto, TimerQuery, TunerHostInfo,
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

/// The buffered file behind an open tuner live stream.
///
/// Port of what `LiveStream.GetStream()` opens: the `{transcode}/{uniqueId}.ts`
/// file the tuner copy task writes, plus the instant the stream was opened.
/// Upstream seeks a reader that arrives more than [`TAIL_SEEK_AFTER_SECONDS`]
/// late to [`TAIL_SEEK_BYTES`] from the end, so it joins the broadcast live
/// instead of replaying the buffer from the start.
#[derive(Debug, Clone)]
pub struct LiveStreamFile {
    /// The temp file the copy task appends tuner bytes to.
    pub path: std::path::PathBuf,
    /// When the stream was opened (C# `ILiveStream.DateOpened`).
    pub opened_at: DateTime<Utc>,
}

/// How stale an open live stream must be before a new reader starts at the tail
/// rather than the head of the buffer (C# `LiveStream.GetStream`'s
/// `(DateTime.UtcNow - DateOpened).TotalSeconds > 10`).
pub const TAIL_SEEK_AFTER_SECONDS: i64 = 10;

/// How far from the end of the buffer such a late reader starts, in bytes (C#
/// `LiveStream.GetStream`'s `TrySeek(stream, -20000)`).
pub const TAIL_SEEK_BYTES: i64 = 20_000;

/// The service name Jellyfin's built-in Live TV service reports.
///
/// Port of `DefaultLiveTvService.ServiceName`; it is what every timer, series
/// timer and channel carries as `ServiceName`.
pub const LIVE_TV_SERVICE_NAME: &str = "Emby";

/// The salt every internal Live TV id derivation appends before hashing.
///
/// Port of `LiveTvDtoService.InternalVersionNumber` (v10.11.8
/// `LiveTvDtoService.cs:28`).
pub const LIVE_TV_INTERNAL_VERSION_NUMBER: &str = "4";

/// The internal id `LiveTvDtoService` derives for a series timer with the given
/// external id.
///
/// Port of `LiveTvDtoService.GetInternalSeriesTimerId` (v10.11.8
/// `LiveTvDtoService.cs:417-421`):
/// `(ServiceName + externalId + InternalVersionNumber).ToLowerInvariant().GetMD5()`.
#[must_use]
pub fn internal_series_timer_id(external_id: &str) -> String {
    ferrofin_common::extensions::get_md5(
        &format!("{LIVE_TV_SERVICE_NAME}{external_id}{LIVE_TV_INTERNAL_VERSION_NUMBER}")
            .to_lowercase(),
    )
    .simple()
    .to_string()
}

/// The defaults a new timer starts from, before any programme is applied.
///
/// Port of `DefaultLiveTvService.GetNewTimerDefaultsAsync` +
/// `LiveTvManager.GetNewTimerDefaultsInternal`: record at any time on this
/// channel only, new episodes only, every day of the week, keep until deleted,
/// and the configured padding (which the caller supplies, since only the real
/// manager can read it).
#[must_use]
pub fn new_timer_defaults(
    pre_padding_seconds: i32,
    post_padding_seconds: i32,
) -> SeriesTimerInfoDto {
    let record_new_only = true;
    SeriesTimerInfoDto {
        base: ferrofin_model::live_tv::BaseTimerInfoDto {
            // `LiveTvManager.GetNewTimerDefaultsInternal` nulls
            // `SeriesTimerInfo.Id`, but `LiveTvDtoService.GetSeriesTimerInfoDto`
            // then derives the DTO id from it unconditionally
            // (`GetInternalSeriesTimerId(info.Id).ToString("N")`), so the empty
            // external id hashes to one fixed value — not null, and not
            // per-instance.
            id: Some(internal_series_timer_id("")),
            type_: Some("SeriesTimer".to_owned()),
            service_name: Some(LIVE_TV_SERVICE_NAME.to_owned()),
            pre_padding_seconds: pre_padding_seconds.max(0),
            post_padding_seconds: post_padding_seconds.max(0),
            keep_until: KeepUntil::UntilDeleted,
            // C# never assigns Start/End on the defaults path, so the CLR
            // `DateTime` default is what serializes — `0001-01-01T00:00:00Z`,
            // NOT the Unix epoch that `chrono`'s `Default` would give.
            start_date: ferrofin_model::json::datetime::dotnet_min(),
            end_date: ferrofin_model::json::datetime::dotnet_min(),
            ..ferrofin_model::live_tv::BaseTimerInfoDto::default()
        },
        record_any_channel: false,
        record_any_time: true,
        record_new_only,
        skip_episodes_in_library: record_new_only,
        days: vec![
            DayOfWeek::Sunday,
            DayOfWeek::Monday,
            DayOfWeek::Tuesday,
            DayOfWeek::Wednesday,
            DayOfWeek::Thursday,
            DayOfWeek::Friday,
            DayOfWeek::Saturday,
        ],
        day_pattern: Some(DayPattern::Daily),
        ..SeriesTimerInfoDto::default()
    }
}

/// Selects the timers a [`TimerQuery`] asks for, in start-date order.
///
/// Port of `LiveTvManager.GetTimersInternal`: the service hands back every
/// timer and the manager filters and orders them in memory, so this is the
/// whole implementation rather than a fallback.
#[must_use]
pub fn filter_timers(timers: Vec<TimerInfoDto>, query: &TimerQuery) -> Vec<TimerInfoDto> {
    let matches_opt = |wanted: Option<&str>, actual: Option<&str>| match wanted
        .map(str::trim)
        .filter(|w| !w.is_empty())
    {
        Some(wanted) => actual.is_some_and(|a| a.eq_ignore_ascii_case(wanted)),
        None => true,
    };
    let mut selected: Vec<TimerInfoDto> = timers
        .into_iter()
        .filter(|timer| {
            // `IsActive` is "recording right now"; `IsScheduled` is "not yet
            // started" — upstream compares the status, not the clock.
            query
                .is_active
                .is_none_or(|active| (timer.status == RecordingStatus::InProgress) == active)
                && query
                    .is_scheduled
                    .is_none_or(|scheduled| (timer.status == RecordingStatus::New) == scheduled)
                // Clients echo back whatever spelling they were given, so the
                // comparison is on the parsed guid, not its text.
                && query
                    .channel_id
                    .as_deref()
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .is_none_or(|wanted| {
                        Uuid::parse_str(wanted).is_ok_and(|wanted| wanted == timer.base.channel_id)
                    })
                && matches_opt(
                    query.series_timer_id.as_deref(),
                    timer.series_timer_id.as_deref(),
                )
                && matches_opt(query.id.as_deref(), timer.base.id.as_deref())
        })
        .collect();
    selected.sort_by_key(|timer| timer.base.start_date);
    selected
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

    /// Queries the "recommended"/"On Now" program list.
    ///
    /// Port of `LiveTvManager.GetRecommendedProgramsAsync`. The contract:
    /// implementations delegate to [`Self::get_programs`] unless
    /// `is_airing == Some(true)`, and otherwise force a StartDate-ascending
    /// fetch of `max(limit * 4, 200)` rows, rank each start-date's airings by
    /// the recommendation score (live, non-repeat series, and the caller's
    /// channel likes/favourite/play count), take `limit`, and report the
    /// fetched pool size as the total.
    async fn get_recommended_programs(
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

    /// The guide's advertised date range.
    ///
    /// Port of `IGuideManager.GetGuideInfo`: `now .. now + GuideDays`, where
    /// `GuideDays` is the dashboard's Live TV setting clamped to `1..=14`. It
    /// lives on the manager rather than in the handler because it must be the
    /// same day count the guide *ingest* window uses — advertising a range the
    /// stored guide does not cover is how a client ends up scrolling into an
    /// empty week.
    async fn get_guide_info(&self) -> Result<GuideInfo, ServiceError>;

    /// Resolves a channel id to the tuner stream URL that plays it, or `None`
    /// when the channel is unknown.
    async fn get_channel_stream_url(&self, id: Uuid) -> Result<Option<String>, ServiceError>;

    // ---- live streams ----------------------------------------------------

    /// The playable media sources for a Live TV channel, or empty when the id
    /// is not a known channel.
    ///
    /// Port of `LiveTvMediaSourceProvider.GetChannelMediaSources` →
    /// `M3UTunerHost.CreateMediaSourceInfo` + `Normalize`: an unopened tuner
    /// source (`RequiresOpening`/`RequiresClosing`, placeholder `Index = -1`
    /// streams, the tuner's `RequiredHttpHeaders`), which the media-source
    /// manager then stamps with the provider-prefixed `OpenToken`.
    ///
    /// The default reports no sources — the "no tuner configured" state.
    async fn get_channel_media_sources(
        &self,
        id: Uuid,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        let _ = id;
        Ok(Vec::new())
    }

    /// Opens (or joins) the tuner stream for a channel, returning the opened
    /// media source: its `Path` is the
    /// `/LiveTv/LiveStreamFiles/{uniqueId}/stream.ts` URL the buffered copy is
    /// served from, and its `LiveStreamId` is the handle
    /// [`close_channel_stream`](Self::close_channel_stream) takes.
    ///
    /// Port of `DefaultLiveTvService.GetChannelStreamWithDirectStreamProvider`
    /// (join an open stream with the same `OriginalStreamId`, else open a new
    /// one) + `M3UTunerHost.GetChannelStream` + `SharedHttpStream.Open`.
    ///
    /// The default reports the channel as unknown.
    async fn open_channel_stream(
        &self,
        channel_id: Uuid,
        media_source_id: Option<&str>,
    ) -> Result<MediaSourceInfo, ServiceError> {
        let _ = (channel_id, media_source_id);
        Err(ServiceError::not_found("live tv channel stream"))
    }

    /// The buffered file behind the open live stream with this unique id, or
    /// `None` when no such stream is open.
    ///
    /// Port of `MediaSourceManager.GetLiveStreamInfoByUniqueId(...)` +
    /// `ILiveStream.GetStream()` — what
    /// `GET /LiveTv/LiveStreamFiles/{streamId}/stream.{container}` serves.
    ///
    /// The default reports nothing open.
    async fn get_live_stream_file(
        &self,
        unique_id: &str,
    ) -> Result<Option<LiveStreamFile>, ServiceError> {
        let _ = unique_id;
        Ok(None)
    }

    /// Releases one consumer of an open live stream, closing the tuner
    /// connection and deleting its buffer once the last one goes. Reports
    /// whether that was the last consumer — i.e. whether the stream is now
    /// really gone, or still serving somebody else.
    ///
    /// Port of the tuner half of `MediaSourceManager.CloseLiveStream`
    /// (`ConsumerCount--`, then `liveStream.Close()` at zero).
    ///
    /// The default reports "gone" — there was nothing open to close.
    async fn close_channel_stream(&self, live_stream_id: &str) -> Result<bool, ServiceError> {
        let _ = live_stream_id;
        Ok(true)
    }

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

    /// The defaults a client seeds a new timer form with, for a programme or in
    /// general.
    ///
    /// Port of `LiveTvManager.GetNewTimerDefaults(programId)`: the standing
    /// defaults, then the programme's own name/overview/channel/dates when one
    /// is named.
    ///
    /// The default reports the standing defaults with no padding — the right
    /// answer when nothing is configured.
    async fn get_new_timer_defaults(
        &self,
        program_id: Option<Uuid>,
    ) -> Result<SeriesTimerInfoDto, ServiceError> {
        let _ = program_id;
        Ok(new_timer_defaults(0, 0))
    }

    /// Lists the timers a query selects.
    ///
    /// Port of `LiveTvManager.GetTimers`. The default filters the full list in
    /// memory, which is what upstream does — see [`filter_timers`].
    async fn get_timers_matching(
        &self,
        query: &TimerQuery,
    ) -> Result<Vec<TimerInfoDto>, ServiceError> {
        Ok(filter_timers(self.get_timers().await?, query))
    }

    // ---- DVR: recordings -------------------------------------------------

    /// Lists recordings as `BaseItemDto`s (`Type = "Recording"`).
    async fn get_recordings(&self) -> Result<QueryResult<BaseItemDto>, ServiceError>;

    /// Lists the recordings a query selects, projected for `user`.
    ///
    /// Port of `LiveTvManager.GetRecordingsAsync`: the in-progress/status/
    /// channel/series-timer filters and paging, then the DTO projection with
    /// the list-path `RemoveFields` strip and the recording post-pass.
    ///
    /// The default ignores the query — the "nothing recorded" state.
    async fn get_recordings_matching(
        &self,
        query: &RecordingQuery,
        user: Option<&UserEntity>,
        options: &DtoOptions,
    ) -> Result<QueryResult<BaseItemDto>, ServiceError> {
        let _ = (query, user, options);
        self.get_recordings().await
    }

    /// The file a capture is writing right now, keyed by the FIRING TIMER's id
    /// — which is what `GET /LiveTv/LiveRecordings/{recordingId}/stream` takes
    /// (upstream's `ActiveRecordingInfo.Id` is `timer.Id`, not the recording's).
    ///
    /// Port of `RecordingsManager.GetActiveRecordingPath`. The default reports
    /// nothing recording.
    async fn get_active_recording_path(
        &self,
        timer_id: &str,
    ) -> Result<Option<String>, ServiceError> {
        let _ = timer_id;
        Ok(None)
    }

    /// The media sources for a recording: while it is being captured, the
    /// growing file plus the `EncoderPath` a transcode reads it through;
    /// afterwards, the finished file.
    ///
    /// Port of `MediaSourceManager.GetRecordingStreamMediaSources` (reached
    /// from `LiveTvMediaSourceProvider.GetMediaSources` when the item has an
    /// active recording). The default reports none.
    async fn get_recording_media_sources(
        &self,
        recording_id: Uuid,
    ) -> Result<Vec<MediaSourceInfo>, ServiceError> {
        let _ = recording_id;
        Ok(Vec::new())
    }

    /// Arms every persisted timer, so a server restart does not lose the
    /// recordings it had scheduled.
    ///
    /// Port of `TimerManager.RestartTimers`, which upstream runs when the Live
    /// TV service starts. The default has no timers to arm.
    async fn start_dvr(&self) -> Result<(), ServiceError> {
        Ok(())
    }

    /// Gets a single recording by id, or `None` when unknown.
    async fn get_recording(&self, id: Uuid) -> Result<Option<BaseItemDto>, ServiceError>;

    /// The on-disk path of a recording's captured file, or `None` when the
    /// recording is unknown or has no file yet. Backs
    /// `GET /LiveTv/LiveRecordings/{recordingId}/stream`.
    async fn get_recording_path(&self, id: Uuid) -> Result<Option<String>, ServiceError>;

    /// Deletes a recording (its DB row and, when present, its file).
    async fn delete_recording(&self, id: Uuid) -> Result<(), ServiceError>;

    // ---- Schedules Direct ------------------------------------------------

    /// The Schedules Direct "available countries" document, as the raw JSON
    /// bytes SD served (the client parses them; the server never does).
    ///
    /// Port of `ISchedulesDirectService.GetAvailableCountries`
    /// (`Jellyfin.LiveTv/Listings/SchedulesDirect.cs`): served from a
    /// process-memory copy, else from the on-disk cache file while it is within
    /// its TTL, else fetched from SD (no account needed) and cached both ways.
    /// An upstream transport/status failure is a backend error (HTTP `500`),
    /// as `EnsureSuccessStatusCode` throwing is upstream.
    async fn get_schedules_direct_countries(&self) -> Result<Vec<u8>, ServiceError>;
}

fn _assert_object_safe_live_tv_manager(_: &dyn LiveTvManager) {}

#[cfg(test)]
mod new_timer_defaults_tests {
    use super::{internal_series_timer_id, new_timer_defaults};

    /// `GET /LiveTv/Timers/Defaults` serializes the C# defaults verbatim. The
    /// two values that used to diverge are asserted on the WIRE, because both
    /// are serialization artifacts: `Start/EndDate` come from an unassigned
    /// .NET `DateTime` (`0001-01-01`, not the Unix epoch), and `Id` is derived
    /// by `LiveTvDtoService` from the nulled external id rather than left null.
    #[test]
    fn serializes_the_dotnet_defaults() {
        let v = serde_json::to_value(new_timer_defaults(0, 0)).expect("serializes");
        assert_eq!(v["StartDate"], "0001-01-01T00:00:00.0000000Z");
        assert_eq!(v["EndDate"], "0001-01-01T00:00:00.0000000Z");
        assert_eq!(v["Id"], "eb075d6a62e2edc6b764a304633d33c0");
        assert_eq!(v["Type"], "SeriesTimer");
        assert_eq!(v["ServiceName"], "Emby");
        assert_eq!(v["DayPattern"], "Daily");
        assert_eq!(v["Days"].as_array().expect("Days is an array").len(), 7);
        assert_eq!(v["KeepUntil"], "UntilDeleted");
    }

    /// `MD5("emby4")` over UTF-16LE, read as a .NET `Guid` — the value the live
    /// Jellyfin 10.11.8 server returns for this endpoint.
    #[test]
    fn internal_series_timer_id_matches_the_csharp_derivation() {
        assert_eq!(
            internal_series_timer_id(""),
            "eb075d6a62e2edc6b764a304633d33c0"
        );
    }
}
