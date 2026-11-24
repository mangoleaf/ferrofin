//! Adoption import of Jellyfin's DVR schedule: the timers and series timers
//! `TimerManager` and `SeriesTimerManager` keep in `{DataPath}/livetv/timers.json`
//! and `seriestimers.json` (v12.4 `Jellyfin.LiveTv/Timers/TimerManager.cs:25`,
//! `SeriesTimerManager.cs:16`), which no database table holds.
//!
//! Each entry is stored as the DTO `LiveTvDtoService.GetTimerInfoDto` and
//! `GetSeriesTimerInfoDto` (v12.4 `LiveTvDtoService.cs:52-159`) build from it:
//! the internal ids derived from the external ones the file holds, which stay
//! on the DTO as its `External*` ids. So the first guide refresh finds each
//! timer's programme (`RefreshTimers`), and a series timer's fan-out finds the
//! timers it made (`(seriesTimer.Id + programme.ExternalId).GetMD5()`).
//! `IsManual` and a series timer's `SeriesId` go to their columns, and a
//! completed timer's `RecordingPath` to the recording row "Keep up to" reads.
//! It is the reverse of the export's mapping
//! (`ferrofin_core::config_export::livetv`).

use std::path::Path;

use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_model::dto::DayOfWeek;
use ferrofin_model::live_tv::{
    BaseTimerInfoDto, KeepUntil, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto,
};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::stubs::{LIVE_TV_SERVICE_NAME, internal_series_timer_id, internal_timer_id};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use uuid::Uuid;

use super::{day_pattern, internal_channel_id, internal_program_id};
use crate::dvr::TimerRecordingInfo;

/// The `FerrofinMeta` key recording that the import has run (`1`), or that it
/// started on the database's first boot and is owed again (`pending`).
const META_KEY: &str = "livetv_timers_imported_v1";

/// The key `ferrofin_core::live_tv_import` sets on a database's first boot,
/// later in the boot than this import runs: a database that has it was booted
/// by a Ferrofin before.
const CONFIG_IMPORTED_KEY: &str = "livetv_config_imported_v1";

/// What [`import_jellyfin_timers`] stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportedTimers {
    /// The timers, completed ones included.
    pub timers: usize,
    /// The completed recordings among them, each with its file.
    pub recordings: usize,
    /// The series timers.
    pub series_timers: usize,
}

/// `TimerInfo` as `timers.json` holds it (`JsonDefaults.Options`: members as
/// declared, enums by name), the members the import stores.
#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)] // one per stored member
struct TimerInfo {
    id: String,
    series_timer_id: Option<String>,
    channel_id: Option<String>,
    program_id: Option<String>,
    name: Option<String>,
    overview: Option<String>,
    #[serde(with = "ferrofin_model::json::datetime")]
    start_date: DateTime<Utc>,
    #[serde(with = "ferrofin_model::json::datetime")]
    end_date: DateTime<Utc>,
    status: RecordingStatus,
    pre_padding_seconds: i32,
    post_padding_seconds: i32,
    is_pre_padding_required: bool,
    is_post_padding_required: bool,
    is_manual: bool,
    priority: i32,
    season_number: Option<i32>,
    episode_number: Option<i32>,
    is_movie: bool,
    production_year: Option<i32>,
    episode_title: Option<String>,
    is_program_series: bool,
    is_repeat: bool,
    tags: Vec<String>,
    recording_path: Option<String>,
    keep_until: KeepUntil,
}

/// `SeriesTimerInfo` as `seriestimers.json` holds it, the members the import
/// stores.
#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase", default)]
#[allow(clippy::struct_excessive_bools)] // one per stored member
struct SeriesTimerInfo {
    id: String,
    channel_id: Option<String>,
    program_id: Option<String>,
    name: Option<String>,
    overview: Option<String>,
    #[serde(with = "ferrofin_model::json::datetime")]
    start_date: DateTime<Utc>,
    #[serde(with = "ferrofin_model::json::datetime")]
    end_date: DateTime<Utc>,
    record_any_time: bool,
    record_any_channel: bool,
    keep_up_to: i32,
    keep_until: KeepUntil,
    /// `true` unless the file says otherwise, as `SeriesTimerInfo`'s
    /// constructor sets it.
    #[serde(default = "skip_episodes_in_library")]
    skip_episodes_in_library: bool,
    record_new_only: bool,
    days: Vec<DayOfWeek>,
    priority: i32,
    pre_padding_seconds: i32,
    post_padding_seconds: i32,
    is_pre_padding_required: bool,
    is_post_padding_required: bool,
    series_id: Option<String>,
}

/// `SeriesTimerInfo.SkipEpisodesInLibrary`'s constructor value.
fn skip_episodes_in_library() -> bool {
    true
}

/// `id` when it is not empty (`string.IsNullOrEmpty`).
fn given(id: Option<&str>) -> Option<&str> {
    id.filter(|id| !id.is_empty())
}

/// `GetInternalProgramId(programId).ToString("N")`, for a programme id that
/// is not empty.
fn program_item_id(program_id: Option<&str>) -> Option<String> {
    given(program_id).map(|id| internal_program_id(id).simple().to_string())
}

impl TimerInfo {
    /// `GetTimerInfoDto(info, …)`'s stored half: what it derives from the
    /// entry, without the programme and channel facts each read adds.
    fn dto(&self, server_id: &str) -> TimerInfoDto {
        TimerInfoDto {
            base: BaseTimerInfoDto {
                id: Some(internal_timer_id(&self.id)),
                type_: Some("Timer".to_owned()),
                server_id: Some(server_id.to_owned()),
                external_id: Some(self.id.clone()),
                // Upstream derives a channel id from an empty one too, but its
                // guide refresh leaves a timer naming no channel alone
                // (`IsNullOrWhiteSpace(timer.ChannelId)`), which is what the
                // nil id tells Ferrofin's.
                channel_id: self
                    .channel_id
                    .as_deref()
                    .filter(|id| !id.trim().is_empty())
                    .map_or(Uuid::nil(), internal_channel_id),
                external_channel_id: self.channel_id.clone(),
                program_id: program_item_id(self.program_id.as_deref()),
                external_program_id: self.program_id.clone(),
                name: self.name.clone(),
                overview: self.overview.clone(),
                start_date: self.start_date,
                end_date: self.end_date,
                service_name: Some(LIVE_TV_SERVICE_NAME.to_owned()),
                priority: self.priority,
                pre_padding_seconds: self.pre_padding_seconds,
                post_padding_seconds: self.post_padding_seconds,
                is_pre_padding_required: self.is_pre_padding_required,
                is_post_padding_required: self.is_post_padding_required,
                keep_until: self.keep_until,
                ..BaseTimerInfoDto::default()
            },
            status: self.status,
            series_timer_id: given(self.series_timer_id.as_deref()).map(internal_series_timer_id),
            external_series_timer_id: self.series_timer_id.clone(),
            run_time_ticks: Some((self.end_date - self.start_date).num_milliseconds() * 10_000),
            program_info: None,
        }
    }

    /// The recording row's facts: the timer's, and the programme's that
    /// `CopyProgramInfoToTimerInfo` copied onto it, `IsKids`, `IsSports`,
    /// `IsNews`, `IsLive` and `IsPremiere` read from its tags as
    /// `TimerInfo`'s getters read them.
    fn recording_info(&self, dto: &TimerInfoDto) -> TimerRecordingInfo {
        let tagged = |tag: &str| self.tags.iter().any(|t| t.eq_ignore_ascii_case(tag));
        TimerRecordingInfo {
            episode_title: self.episode_title.clone(),
            season_number: self.season_number,
            episode_number: self.episode_number,
            production_year: self.production_year,
            is_program_series: self.is_program_series,
            is_movie: self.is_movie,
            is_kids: tagged("Kids"),
            is_sports: tagged("Sports"),
            is_news: tagged("News"),
            is_live: tagged("Live"),
            is_repeat: self.is_repeat,
            is_premiere: tagged("Premiere"),
            ..TimerRecordingInfo::from_timer(dto)
        }
    }
}

impl SeriesTimerInfo {
    /// `GetSeriesTimerInfoDto(info, …)`'s stored half, as
    /// [`TimerInfo::dto`] is `GetTimerInfoDto`'s.
    fn dto(&self, server_id: &str) -> SeriesTimerInfoDto {
        SeriesTimerInfoDto {
            base: BaseTimerInfoDto {
                id: Some(internal_series_timer_id(&self.id)),
                type_: Some("SeriesTimer".to_owned()),
                server_id: Some(server_id.to_owned()),
                external_id: Some(self.id.clone()),
                channel_id: given(self.channel_id.as_deref())
                    .map_or(Uuid::nil(), internal_channel_id),
                external_channel_id: self.channel_id.clone(),
                program_id: program_item_id(self.program_id.as_deref()),
                external_program_id: self.program_id.clone(),
                name: self.name.clone(),
                overview: self.overview.clone(),
                start_date: self.start_date,
                end_date: self.end_date,
                service_name: Some(LIVE_TV_SERVICE_NAME.to_owned()),
                priority: self.priority,
                pre_padding_seconds: self.pre_padding_seconds,
                post_padding_seconds: self.post_padding_seconds,
                is_pre_padding_required: self.is_pre_padding_required,
                is_post_padding_required: self.is_post_padding_required,
                keep_until: self.keep_until,
                ..BaseTimerInfoDto::default()
            },
            record_any_time: self.record_any_time,
            skip_episodes_in_library: self.skip_episodes_in_library,
            record_any_channel: self.record_any_channel,
            keep_up_to: self.keep_up_to,
            record_new_only: self.record_new_only,
            day_pattern: day_pattern(&self.days),
            days: self.days.clone(),
            ..SeriesTimerInfoDto::default()
        }
    }
}

/// The entries of the `ItemDataProvider` file at `path`: none when it is
/// missing, or when it does not deserialize, which is logged, as
/// `ItemDataProvider.EnsureLoaded` reads such a file as empty.
///
/// # Errors
///
/// Returns [`ServiceError::Backend`] when the file is there but cannot be
/// read (its permissions, say), which a later boot may fix.
async fn read_entries<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>, ServiceError> {
    let bytes = match tokio::fs::read(path).await {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(ServiceError::backend(format!(
                "could not read {}: {e}",
                path.display()
            )));
        }
    };
    Ok(serde_json::from_slice(&bytes).unwrap_or_else(|error| {
        tracing::warn!(path = %path.display(), %error, "live tv: jellyfin's timers do not parse; none of them are imported");
        Vec::new()
    }))
}

/// Imports Jellyfin's timers and series timers from `{data_dir}/livetv/`
/// into `db`, once, on the first boot of a database, as the DTOs of the
/// server `server_id`. Run it before
/// [`start_dvr`](ferrofin_traits::stubs::LiveTvManager::start_dvr) arms the
/// stored timers, before the boot's program-data path rebase (which moves a
/// recording's file onto this server's directories), and before
/// `ferrofin_core`'s Live TV configuration import marks the database.
///
/// On a database a Ferrofin booted before, the files are what Jellyfin left
/// at its adoption and the schedule since is Ferrofin's, so the import only
/// marks itself done. A file that is missing or does not parse imports
/// nothing, and the import is still done.
///
/// # Errors
///
/// Returns a [`ServiceError`] when a file is there but cannot be read, or
/// when the database rejects a read or a write. The import is then owed again
/// at the next boot, which writes the same rows over the ones this one wrote.
pub async fn import_jellyfin_timers(
    db: &Database,
    server_id: &str,
    data_dir: &Path,
) -> Result<ImportedTimers, ServiceError> {
    let mut imported = ImportedTimers::default();
    match db.meta_get(META_KEY).await?.as_deref() {
        Some("1") => return Ok(imported),
        Some(_) => {}
        None if db.meta_get(CONFIG_IMPORTED_KEY).await?.is_some() => {
            db.meta_set(META_KEY, "1").await?;
            return Ok(imported);
        }
        None => db.meta_set(META_KEY, "pending").await?,
    }
    let dir = data_dir.join("livetv");
    for series in read_entries::<SeriesTimerInfo>(&dir.join("seriestimers.json")).await? {
        crate::dvr_repository::upsert_series_timer(
            db,
            &series.dto(server_id),
            &series.id,
            given(series.series_id.as_deref()),
        )
        .await?;
        imported.series_timers += 1;
    }
    for timer in read_entries::<TimerInfo>(&dir.join("timers.json")).await? {
        let dto = timer.dto(server_id);
        let id = crate::dvr_repository::upsert_timer(db, &dto, timer.is_manual).await?;
        imported.timers += 1;
        let path = given(timer.recording_path.as_deref());
        if let (RecordingStatus::Completed, Some(path)) = (timer.status, path) {
            // A retried import replaces the row the failed one wrote.
            crate::dvr_repository::delete_timer_recordings(db, &id).await?;
            let recording = Uuid::new_v4();
            crate::dvr_repository::insert_recording(
                db,
                recording,
                &timer.recording_info(&dto),
                Path::new(path),
                // When its capture began.
                timer.start_date,
            )
            .await?;
            crate::dvr_repository::finish_recording(
                db,
                recording,
                RecordingStatus::Completed,
                Some(path),
            )
            .await?;
            imported.recordings += 1;
        }
    }
    db.meta_set(META_KEY, "1").await?;
    Ok(imported)
}
