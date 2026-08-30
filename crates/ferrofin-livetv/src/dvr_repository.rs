//! Row reads and writes for the DVR — timers and recordings.
//!
//! The SQL boundary keeps raw queries in repository modules; the scheduler and
//! the recorder in [`crate::dvr`] and [`crate::manager`] compose these with the
//! capture logic and the DTO-service projection.

use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::live_tv::{RecordingQuery, RecordingStatus, SeriesTimerInfoDto, TimerInfoDto};
use sqlx::{QueryBuilder, Sqlite};
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;

use crate::projection::RecordingRow;

/// The columns a recording read returns, joined to its channel for the
/// `ChannelName` `AddInfoToRecordingDto` sets from the channel item.
const RECORDING_SELECT: &str = r#"SELECT r."Id",r."ChannelId",r."TimerId",r."SeriesTimerId",
              r."Name",r."Overview",r."StartDate",r."EndDate",r."Status",r."Path",
              r."DateCreated",r."EpisodeTitle",r."ProductionYear",r."SeasonNumber",
              r."EpisodeNumber",r."ProgramId",r."ExternalProgramId",
              r."PrePaddingSeconds",r."PostPaddingSeconds",
              r."IsMovie",r."IsSeries",r."IsNews",r."IsKids",r."IsSports",
              r."IsLive",r."IsRepeat",r."IsPremiere",
              c."Name" AS "ChannelName"
       FROM "FerrofinLiveTvRecordings" r
       LEFT JOIN "FerrofinLiveTvChannels" c ON c."Id" = r."ChannelId""#;

/// Inserts or replaces one timer, storing the whole DTO as JSON so a `GET`
/// round-trips exactly what was posted.
///
/// `is_manual` is `TimerInfo.IsManual` — whether a person asked for this exact
/// recording rather than a series timer scheduling it. It is STICKY-TRUE: an
/// update raises the stored flag but never clears it (`MAX` in the `ON CONFLICT`
/// clause below). That is exactly what upstream does with the field, because
/// every assignment in `DefaultLiveTvService` writes `true` and none writes
/// `false` — `:178` on a manual cancel, `:227` on reviving a cancelled timer by
/// hand, `:255` on a manual create, `:302` when a series timer adopts an
/// existing timer — and the one read-back (`:745`,
/// `timer.IsManual = existingTimer.IsManual`) copies the stored value forward
/// rather than resetting it. Writing it on INSERT only would silently drop
/// every one of those four, which is what `persist_manual_timer` on an
/// already-stored row means.
///
/// # Errors
///
/// Fails when the write fails, or when the DTO cannot be serialized.
pub async fn upsert_timer(
    db: &Database,
    timer: &TimerInfoDto,
    is_manual: bool,
) -> Result<String, ServiceError> {
    let id = timer.base.id.clone().unwrap_or_default();
    let data = serde_json::to_string(timer).map_err(|e| {
        ServiceError::from(crate::error::LiveTvError::serialize("serialize timer", e))
    })?;
    sqlx::query(
        r#"INSERT INTO "FerrofinLiveTvTimers"
           ("Id","ChannelId","ProgramId","SeriesTimerId","Name","StartDate","EndDate","Status",
            "PrePaddingSeconds","PostPaddingSeconds","Data","IsManual")
           VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)
           ON CONFLICT("Id") DO UPDATE SET
             "ChannelId"=excluded."ChannelId","ProgramId"=excluded."ProgramId",
             "SeriesTimerId"=excluded."SeriesTimerId","Name"=excluded."Name",
             "StartDate"=excluded."StartDate","EndDate"=excluded."EndDate",
             "Status"=excluded."Status","Data"=excluded."Data",
             "IsManual"=MAX("FerrofinLiveTvTimers"."IsManual",excluded."IsManual")"#,
    )
    .bind(&id)
    .bind(guid_to_db(timer.base.channel_id))
    .bind(&timer.base.program_id)
    .bind(&timer.series_timer_id)
    .bind(timer.base.name.clone().unwrap_or_default())
    .bind(datetime_to_db(timer.base.start_date))
    .bind(datetime_to_db(timer.base.end_date))
    .bind(status_name(timer.status))
    .bind(timer.base.pre_padding_seconds)
    .bind(timer.base.post_padding_seconds)
    .bind(&data)
    .bind(i32::from(is_manual))
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(id)
}

/// Whether the timer with this id was created by hand (`TimerInfo.IsManual`).
///
/// A timer that is not there is not manual — upstream reads the flag off a
/// `TimerInfo` it already holds, so the question only arises for a row that
/// exists.
///
/// # Errors
///
/// Fails when the read fails.
pub async fn timer_is_manual(db: &Database, id: &str) -> Result<bool, ServiceError> {
    let flag: Option<i64> =
        sqlx::query_scalar(r#"SELECT "IsManual" FROM "FerrofinLiveTvTimers" WHERE "Id" = ?1"#)
            .bind(id)
            .fetch_optional(db.pool())
            .await
            .map_err(db_err)?;
    Ok(flag.is_some_and(|f| f != 0))
}

/// One stored series timer's row: the published DTO JSON, the external id it was
/// derived from, and the listings series id the fan-out queries with.
///
/// # Errors
///
/// Fails when the read fails.
pub async fn series_timer_row(
    db: &Database,
    id: &str,
) -> Result<Option<(SeriesTimerInfoDto, String, Option<String>)>, ServiceError> {
    let row: Option<(String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT "Data","ExternalId","SeriesId" FROM "FerrofinLiveTvSeriesTimers"
           WHERE "Id" = ?1"#,
    )
    .bind(id)
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    Ok(row.and_then(|(data, external_id, series_id)| {
        serde_json::from_str(&data)
            .ok()
            .map(|dto| (dto, external_id, series_id))
    }))
}

/// Inserts or replaces one series timer, DTO and identity columns together.
///
/// # Errors
///
/// Fails when the write fails, or when the DTO cannot be serialized.
pub async fn upsert_series_timer(
    db: &Database,
    timer: &SeriesTimerInfoDto,
    external_id: &str,
    series_id: Option<&str>,
) -> Result<(), ServiceError> {
    let data = serde_json::to_string(timer).map_err(|e| {
        ServiceError::from(crate::error::LiveTvError::serialize("serialize timer", e))
    })?;
    sqlx::query(
        r#"INSERT INTO "FerrofinLiveTvSeriesTimers"
           ("Id","ChannelId","ProgramId","Name","Data","ExternalId","SeriesId")
           VALUES (?1,?2,?3,?4,?5,?6,?7)
           ON CONFLICT("Id") DO UPDATE SET
             "ChannelId"=excluded."ChannelId","ProgramId"=excluded."ProgramId",
             "Name"=excluded."Name","Data"=excluded."Data",
             "ExternalId"=excluded."ExternalId","SeriesId"=excluded."SeriesId""#,
    )
    .bind(timer.base.id.clone().unwrap_or_default())
    .bind(guid_to_db(timer.base.channel_id))
    .bind(&timer.base.program_id)
    .bind(timer.base.name.clone().unwrap_or_default())
    .bind(&data)
    .bind(external_id)
    .bind(series_id)
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(())
}

/// The ids of every timer a series timer scheduled.
///
/// # Errors
///
/// Fails when the read fails.
pub async fn timer_ids_for_series(
    db: &Database,
    series_timer_id: &str,
) -> Result<Vec<String>, ServiceError> {
    sqlx::query_scalar(r#"SELECT "Id" FROM "FerrofinLiveTvTimers" WHERE "SeriesTimerId" = ?1"#)
        .bind(series_timer_id)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// The stored timer whose `ProgramId` or `ExternalProgramId` names this
/// programme, or `None` when nothing is scheduled for it.
///
/// Port of `TimerManager.GetTimerByProgramId`, widened to accept either
/// spelling of the programme id, because the two travel together on the DTO.
///
/// # Errors
///
/// Fails when the read fails.
pub async fn timer_for_program(
    db: &Database,
    program_id: &str,
    external_program_id: Option<&str>,
) -> Result<Option<TimerInfoDto>, ServiceError> {
    let data: Option<String> = sqlx::query_scalar(
        r#"SELECT "Data" FROM "FerrofinLiveTvTimers"
           WHERE "ProgramId" IS NOT NULL
             AND ("ProgramId" = ?1 COLLATE NOCASE OR "ProgramId" = ?2 COLLATE NOCASE)
           LIMIT 1"#,
    )
    .bind(program_id)
    .bind(external_program_id.unwrap_or(program_id))
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    Ok(data.and_then(|d| serde_json::from_str(&d).ok()))
}

/// Inserts the row a capture is about to fill.
///
/// # Errors
///
/// Fails when the write fails.
#[allow(clippy::too_many_arguments)] // one argument per stored recording column
pub async fn insert_recording(
    db: &Database,
    recording_id: Uuid,
    timer: &crate::dvr::TimerRecordingInfo,
    path: &std::path::Path,
    created: DateTime<Utc>,
) -> Result<(), ServiceError> {
    sqlx::query(
        r#"INSERT INTO "FerrofinLiveTvRecordings"
           ("Id","ChannelId","TimerId","SeriesTimerId","Name","Overview","StartDate","EndDate",
            "Status","Path","DateCreated","EpisodeTitle","ProductionYear","SeasonNumber",
            "EpisodeNumber","ProgramId","ExternalProgramId","PrePaddingSeconds",
            "PostPaddingSeconds","IsMovie","IsSeries","IsNews","IsKids","IsSports","IsLive",
            "IsRepeat","IsPremiere")
           VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,
                   ?22,?23,?24,?25,?26,?27)"#,
    )
    .bind(guid_to_db(recording_id))
    .bind(guid_to_db(timer.channel_id))
    .bind(&timer.id)
    .bind(&timer.series_timer_id)
    .bind(&timer.name)
    .bind(&timer.overview)
    .bind(datetime_to_db(timer.start_date))
    .bind(datetime_to_db(timer.end_date))
    .bind(status_name(RecordingStatus::InProgress))
    .bind(path.display().to_string())
    .bind(datetime_to_db(created))
    .bind(&timer.episode_title)
    .bind(timer.production_year)
    .bind(timer.season_number)
    .bind(timer.episode_number)
    .bind(&timer.program_id)
    .bind(&timer.external_program_id)
    .bind(timer.pre_padding_seconds)
    .bind(timer.post_padding_seconds)
    .bind(timer.is_movie)
    .bind(timer.is_program_series)
    .bind(timer.is_news)
    .bind(timer.is_kids)
    .bind(timer.is_sports)
    .bind(timer.is_live)
    .bind(timer.is_repeat)
    .bind(timer.is_premiere)
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Moves a recording to its final status, recording where the file ended up.
///
/// # Errors
///
/// Fails when the write fails.
pub async fn finish_recording(
    db: &Database,
    recording_id: Uuid,
    status: RecordingStatus,
    path: Option<&str>,
) -> Result<(), ServiceError> {
    sqlx::query(
        r#"UPDATE "FerrofinLiveTvRecordings" SET "Status" = ?2, "Path" = ?3 WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(recording_id))
    .bind(status_name(status))
    .bind(path)
    .execute(db.writer())
    .await
    .map_err(db_err)?;
    Ok(())
}

/// Deletes one recording row.
///
/// # Errors
///
/// Fails when the write fails.
pub async fn delete_recording(db: &Database, recording_id: Uuid) -> Result<(), ServiceError> {
    sqlx::query(r#"DELETE FROM "FerrofinLiveTvRecordings" WHERE "Id" = ?1"#)
        .bind(guid_to_db(recording_id))
        .execute(db.writer())
        .await
        .map_err(db_err)?;
    Ok(())
}

/// One recording by id, or `None` when unknown.
///
/// # Errors
///
/// Fails when the read fails.
pub async fn recording_row(
    db: &Database,
    recording_id: Uuid,
) -> Result<Option<RecordingRow>, ServiceError> {
    let mut qb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(RECORDING_SELECT);
    qb.push(r#" WHERE r."Id" = "#)
        .push_bind(guid_to_db(recording_id));
    qb.build_query_as()
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)
}

/// The recordings a query selects, newest capture first.
///
/// Port of the filters `LiveTvManager.GetRecordingsAsync` applies:
/// in-progress/status, channel, series timer and the programme kind flags.
///
/// # Errors
///
/// Fails when the read fails.
pub async fn recording_rows(
    db: &Database,
    query: &RecordingQuery,
) -> Result<Vec<RecordingRow>, ServiceError> {
    let mut qb: QueryBuilder<'_, Sqlite> = QueryBuilder::new(RECORDING_SELECT);
    let mut first = true;
    let mut separator = |qb: &mut QueryBuilder<'_, Sqlite>| {
        qb.push(if first { " WHERE " } else { " AND " });
        first = false;
    };

    if let Some(in_progress) = query.is_in_progress {
        separator(&mut qb);
        qb.push(if in_progress {
            r#"r."Status" = 'InProgress'"#
        } else {
            r#"r."Status" <> 'InProgress'"#
        });
    }
    if let Some(status) = query.status {
        separator(&mut qb);
        qb.push(r#"r."Status" = "#).push_bind(status_name(status));
    }
    if let Some(channel_id) = query
        .channel_id
        .as_deref()
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .and_then(|c| Uuid::parse_str(c).ok())
    {
        separator(&mut qb);
        qb.push(r#"r."ChannelId" = "#)
            .push_bind(guid_to_db(channel_id));
    }
    if let Some(series_timer_id) = query
        .series_timer_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        separator(&mut qb);
        qb.push(r#"r."SeriesTimerId" = "#)
            .push_bind(series_timer_id.to_owned());
    }
    for (wanted, column) in [
        (query.is_movie, r#"r."IsMovie""#),
        (query.is_series, r#"r."IsSeries""#),
        (query.is_news, r#"r."IsNews""#),
        (query.is_kids, r#"r."IsKids""#),
        (query.is_sports, r#"r."IsSports""#),
    ] {
        if let Some(wanted) = wanted {
            separator(&mut qb);
            qb.push(column).push(" = ").push_bind(i32::from(wanted));
        }
    }

    // Newest first: a client polling a running capture wants it at the top.
    qb.push(r#" ORDER BY COALESCE(r."DateCreated", r."StartDate") DESC, r."Name""#);
    qb.build_query_as()
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// The `RecordingStatus` name a row stores.
///
/// Port of `RecordingStatus.ToString()`, which is what the wire and the column
/// both carry.
#[must_use]
pub fn status_name(status: RecordingStatus) -> &'static str {
    match status {
        RecordingStatus::New => "New",
        RecordingStatus::InProgress => "InProgress",
        RecordingStatus::Completed => "Completed",
        RecordingStatus::Cancelled => "Cancelled",
        RecordingStatus::ConflictedOk => "ConflictedOk",
        RecordingStatus::ConflictedNotOk => "ConflictedNotOk",
        RecordingStatus::Error => "Error",
    }
}

/// Maps a `sqlx` error into a [`ServiceError`] via `ferrofin-db`'s `DbError`.
fn db_err(e: sqlx::Error) -> ServiceError {
    ServiceError::from(ferrofin_db::DbError::from(e))
}
