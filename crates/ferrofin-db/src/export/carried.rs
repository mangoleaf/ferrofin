//! What the export carries out of Ferrofin's own tables before
//! `drop_objects` removes them (gl-1556, D6 and D7): the Live TV tuners,
//! guide providers, timers and series timers, which Jellyfin keeps in
//! `config/livetv.xml` and `data/livetv/*.json`, and the identity links, which
//! Jellyfin's LDAP-Auth plugin keeps in its settings. The rows are read as
//! stored; `ferrofin-core`'s configuration export writes them as Jellyfin's
//! files.
//!
//! It also counts the Live TV channels and programmes Ferrofin mirrors as
//! Jellyfin items (`LiveTvChannel`, `LiveTvProgram` rows in `BaseItems`),
//! which the export keeps, with their user data, for the loss report.
//!
//! A database from an earlier Ferrofin that lacks a table or a column reads
//! as having none of it: a column added later reads as its default.

use sqlx::SqliteConnection;

use super::paths::{has_column, has_table};
use crate::Result;

/// The stored type of a Live TV channel item.
const CHANNEL_TYPE: &str = "MediaBrowser.Controller.LiveTv.LiveTvChannel";

/// The stored type of a guide programme item.
const PROGRAM_TYPE: &str = "MediaBrowser.Controller.LiveTv.LiveTvProgram";

/// One stored timer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TimerRow {
    /// Its internal id, which its recording names.
    pub id: String,
    /// Its `TimerInfoDto`, as stored.
    pub data: String,
    /// Whether a person asked for it (`TimerInfo.IsManual`).
    pub is_manual: bool,
    /// The recording it made, when it made one.
    pub recording: Option<RecordingRow>,
}

/// The recording a timer made: the facts a completed timer keeps in
/// `timers.json`, which Jellyfin never refreshes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // one per stored flag
pub struct RecordingRow {
    /// Its status (`Completed`, `InProgress`, …).
    pub status: String,
    /// The recorded file, as stored.
    pub path: Option<String>,
    /// The episode's title.
    pub episode_title: Option<String>,
    /// The season number.
    pub season_number: Option<i32>,
    /// The episode number.
    pub episode_number: Option<i32>,
    /// The production year.
    pub production_year: Option<i32>,
    /// Whether it is a film.
    pub is_movie: bool,
    /// Whether the programme is a series.
    pub is_series: bool,
    /// Whether it is news.
    pub is_news: bool,
    /// Whether it is for children.
    pub is_kids: bool,
    /// Whether it is sport.
    pub is_sports: bool,
    /// Whether it was live.
    pub is_live: bool,
    /// Whether it was a repeat.
    pub is_repeat: bool,
    /// Whether it was a premiere.
    pub is_premiere: bool,
}

/// One stored series timer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeriesTimerRow {
    /// Its `SeriesTimerInfoDto`, as stored.
    pub data: String,
    /// The external id it was derived from (empty when the database has none).
    pub external_id: String,
    /// The listings series id its airings are matched by.
    pub series_id: Option<String>,
}

/// One link from an outside identity to a user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IdentityLink {
    /// The authentication provider it belongs to.
    pub provider_id: String,
    /// The user's id, as stored.
    pub user_id: String,
    /// The identity (an LDAP uid).
    pub subject: String,
    /// The hash of the profile image last taken from it.
    pub avatar_hash: Option<String>,
}

/// The Live TV items Ferrofin mirrors in `BaseItems`, which the export keeps.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MirroredLiveTv {
    /// `LiveTvChannel` rows.
    pub channels: u64,
    /// `LiveTvProgram` rows.
    pub programs: u64,
    /// The users' favourite channels: `UserData` rows on a channel with
    /// `IsFavorite` set.
    pub favorite_channels: u64,
}

/// What the export read from Ferrofin's own tables before dropping them.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct CarriedRows {
    /// Each tuner's `TunerHostInfo`, as stored, in id order.
    pub tuner_hosts: Vec<String>,
    /// Each guide provider's `ListingsProviderInfo`, as stored, in id order.
    pub listing_providers: Vec<String>,
    /// The timers, in stored order.
    pub timers: Vec<TimerRow>,
    /// The series timers, in stored order.
    pub series_timers: Vec<SeriesTimerRow>,
    /// Every identity link, in stored order.
    pub identity_links: Vec<IdentityLink>,
    /// The Live TV items kept in the export.
    pub mirrored: MirroredLiveTv,
}

/// Counts only: a tuner's or a guide provider's settings can hold a password
/// (a Schedules Direct login, a tuner URL's), so a debug line never shows them.
impl std::fmt::Debug for CarriedRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CarriedRows")
            .field("tuner_hosts", &self.tuner_hosts.len())
            .field("listing_providers", &self.listing_providers.len())
            .field("timers", &self.timers.len())
            .field("series_timers", &self.series_timers.len())
            .field("identity_links", &self.identity_links.len())
            .field("mirrored", &self.mirrored)
            .finish()
    }
}

/// Reads [`CarriedRows`] from the staged copy.
pub(super) async fn read(conn: &mut SqliteConnection) -> Result<CarriedRows> {
    Ok(CarriedRows {
        tuner_hosts: data_column(conn, "FerrofinLiveTvTunerHosts").await?,
        listing_providers: data_column(conn, "FerrofinLiveTvListingProviders").await?,
        timers: timers(conn).await?,
        series_timers: series_timers(conn).await?,
        identity_links: identity_links(conn).await?,
        mirrored: mirrored(conn).await?,
    })
}

/// The `Data` of every row of `table`, in id order.
async fn data_column(conn: &mut SqliteConnection, table: &str) -> Result<Vec<String>> {
    if !has_table(conn, table).await? {
        return Ok(Vec::new());
    }
    // The table name is one of the two above.
    Ok(sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        r#"SELECT "Data" FROM "{table}" ORDER BY "Id""#
    )))
    .fetch_all(&mut *conn)
    .await?)
}

/// `"{column}"` when `table` has it, else `default` (an SQL literal).
async fn column_or(
    conn: &mut SqliteConnection,
    table: &str,
    column: &str,
    default: &str,
) -> Result<String> {
    Ok(if has_column(conn, table, column).await? {
        format!(r#""{column}""#)
    } else {
        default.to_owned()
    })
}

/// The stored timers, each with the recording it made, if any (the newest,
/// when a crash left more than one).
async fn timers(conn: &mut SqliteConnection) -> Result<Vec<TimerRow>> {
    const TIMERS: &str = "FerrofinLiveTvTimers";
    const RECORDINGS: &str = "FerrofinLiveTvRecordings";
    if !has_table(conn, TIMERS).await? {
        return Ok(Vec::new());
    }
    let manual = column_or(conn, TIMERS, "IsManual", "0").await?;
    let rows: Vec<(String, String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        r#"SELECT "Id", "Data", {manual} FROM "{TIMERS}" ORDER BY rowid"#
    )))
    .fetch_all(&mut *conn)
    .await?;
    let has_recordings = has_table(conn, RECORDINGS).await?;
    let mut timers = Vec::with_capacity(rows.len());
    for (id, data, is_manual) in rows {
        let recording = if has_recordings {
            recording(conn, &id).await?
        } else {
            None
        };
        timers.push(TimerRow {
            id,
            data,
            is_manual: is_manual != 0,
            recording,
        });
    }
    Ok(timers)
}

/// The recording the timer `timer_id` made, the newest when there are several.
async fn recording(conn: &mut SqliteConnection, timer_id: &str) -> Result<Option<RecordingRow>> {
    #[allow(clippy::type_complexity)] // the columns read, in order
    let row: Option<(
        String,
        Option<String>,
        Option<String>,
        Option<i32>,
        Option<i32>,
        Option<i32>,
        bool,
        bool,
        bool,
        bool,
        bool,
        bool,
        bool,
        bool,
    )> = sqlx::query_as(
        r#"SELECT "Status", "Path", "EpisodeTitle", "SeasonNumber", "EpisodeNumber",
                  "ProductionYear", "IsMovie", "IsSeries", "IsNews", "IsKids", "IsSports",
                  "IsLive", "IsRepeat", "IsPremiere"
           FROM "FerrofinLiveTvRecordings" WHERE "TimerId" = ?1
           ORDER BY "StartDate" DESC, rowid DESC LIMIT 1"#,
    )
    .bind(timer_id)
    .fetch_optional(&mut *conn)
    .await?;
    Ok(row.map(
        |(status, path, episode_title, season, episode, year, m, s, n, k, sp, l, r, p)| {
            RecordingRow {
                status,
                path,
                episode_title,
                season_number: season,
                episode_number: episode,
                production_year: year,
                is_movie: m,
                is_series: s,
                is_news: n,
                is_kids: k,
                is_sports: sp,
                is_live: l,
                is_repeat: r,
                is_premiere: p,
            }
        },
    ))
}

/// The stored series timers.
async fn series_timers(conn: &mut SqliteConnection) -> Result<Vec<SeriesTimerRow>> {
    const TABLE: &str = "FerrofinLiveTvSeriesTimers";
    if !has_table(conn, TABLE).await? {
        return Ok(Vec::new());
    }
    let external = column_or(conn, TABLE, "ExternalId", "''").await?;
    let series = column_or(conn, TABLE, "SeriesId", "NULL").await?;
    let rows: Vec<(String, String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        r#"SELECT "Data", {external}, {series} FROM "{TABLE}" ORDER BY rowid"#
    )))
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(data, external_id, series_id)| SeriesTimerRow {
            data,
            external_id,
            series_id,
        })
        .collect())
}

/// Every identity link.
async fn identity_links(conn: &mut SqliteConnection) -> Result<Vec<IdentityLink>> {
    if !has_table(conn, "FerrofinExternalIdentities").await? {
        return Ok(Vec::new());
    }
    let rows: Vec<(String, String, String, Option<String>)> = sqlx::query_as(
        r#"SELECT "ProviderId", "UserId", "Subject", "AvatarHash"
           FROM "FerrofinExternalIdentities" ORDER BY "Id""#,
    )
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(provider_id, user_id, subject, avatar_hash)| IdentityLink {
                provider_id,
                user_id,
                subject,
                avatar_hash,
            },
        )
        .collect())
}

/// The Live TV items in `BaseItems`, and the favourites on its channels.
async fn mirrored(conn: &mut SqliteConnection) -> Result<MirroredLiveTv> {
    if !has_table(conn, "BaseItems").await? {
        return Ok(MirroredLiveTv::default());
    }
    let (channels, programs): (i64, i64) = sqlx::query_as(
        r#"SELECT COALESCE(SUM("Type" = ?1), 0), COALESCE(SUM("Type" = ?2), 0) FROM "BaseItems"
           WHERE "Type" IN (?1, ?2)"#,
    )
    .bind(CHANNEL_TYPE)
    .bind(PROGRAM_TYPE)
    .fetch_one(&mut *conn)
    .await?;
    let favorite_channels: i64 = if has_table(conn, "UserData").await? {
        sqlx::query_scalar(
            r#"SELECT COUNT(*) FROM "UserData" u JOIN "BaseItems" b ON b."Id" = u."ItemId"
               WHERE b."Type" = ?1 AND u."IsFavorite" = 1"#,
        )
        .bind(CHANNEL_TYPE)
        .fetch_one(&mut *conn)
        .await?
    } else {
        0
    };
    Ok(MirroredLiveTv {
        channels: channels.unsigned_abs(),
        programs: programs.unsigned_abs(),
        favorite_channels: favorite_channels.unsigned_abs(),
    })
}

#[cfg(test)]
mod tests {
    use sqlx::ConnectOptions as _;
    use sqlx::sqlite::SqliteConnectOptions;

    use super::{CarriedRows, TimerRow, read};

    /// A database from an earlier Ferrofin, without the identity links
    /// (before 0048), the timers' `IsManual` or the series timers' identity
    /// columns, reads as having none of them; one without Live TV tables or
    /// items reads as empty.
    #[tokio::test]
    async fn a_table_or_column_an_older_database_lacks_reads_as_none() {
        let mut conn = SqliteConnectOptions::new()
            .in_memory(true)
            .connect()
            .await
            .unwrap();
        assert_eq!(read(&mut conn).await.unwrap(), CarriedRows::default());
        for statement in [
            r#"CREATE TABLE "FerrofinLiveTvTimers" ("Id" TEXT, "Data" TEXT)"#,
            r#"CREATE TABLE "FerrofinLiveTvSeriesTimers" ("Id" TEXT, "Data" TEXT)"#,
            r#"INSERT INTO "FerrofinLiveTvTimers" VALUES ('t', '{}')"#,
            r#"INSERT INTO "FerrofinLiveTvSeriesTimers" VALUES ('s', '{"Name":"x"}')"#,
        ] {
            sqlx::query(statement).execute(&mut conn).await.unwrap();
        }
        let carried = read(&mut conn).await.unwrap();
        assert_eq!(
            carried.timers,
            [TimerRow {
                id: "t".to_owned(),
                data: "{}".to_owned(),
                is_manual: false,
                recording: None,
            }]
        );
        assert_eq!(carried.series_timers[0].external_id, "");
        assert_eq!(carried.series_timers[0].series_id, None);
        assert_eq!(carried.identity_links, []);
    }

    /// A debug line counts the rows and never shows their settings.
    #[test]
    fn debug_shows_counts_not_settings() {
        let carried = CarriedRows {
            listing_providers: vec![r#"{"Password":"sd-secret"}"#.to_owned()],
            ..CarriedRows::default()
        };
        let debug = format!("{carried:?}");
        assert!(!debug.contains("sd-secret"), "{debug}");
        assert!(debug.contains("listing_providers: 1"), "{debug}");
    }
}
