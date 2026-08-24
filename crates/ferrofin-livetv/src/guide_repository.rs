//! Row reads for the guide's channel projection paths.
//!
//! The SQL boundary keeps raw queries in repository modules; the manager
//! composes these reads with the DTO-service projection in
//! [`crate::manager::FerrofinLiveTvManager`].

use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use sqlx::Row as _;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;

use crate::projection::{ChannelRow, ChannelUserData};

/// Every channel in the lineup, in stored (`SortIndex`) order, each carrying
/// the guide-derived movie/series/kids flags `GuideManager.RefreshChannels`
/// aggregates onto upstream channel items (`isMovie |= program.IsMovie; ...`,
/// plus the "Kids" tag) — the columns/tag the channel kind filters match on.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn channel_rows(db: &Database) -> Result<Vec<ChannelRow>, ServiceError> {
    sqlx::query_as(
        r#"SELECT "Id","TvgId","Name","Number","ChannelType","DateCreated",
                  EXISTS(SELECT 1 FROM "FerrofinLiveTvPrograms" p
                         WHERE p."ChannelId" = "FerrofinLiveTvChannels"."Id" AND p."IsMovie" = 1) AS "IsMovie",
                  EXISTS(SELECT 1 FROM "FerrofinLiveTvPrograms" p
                         WHERE p."ChannelId" = "FerrofinLiveTvChannels"."Id" AND p."IsSeries" = 1) AS "IsSeries",
                  EXISTS(SELECT 1 FROM "FerrofinLiveTvPrograms" p
                         WHERE p."ChannelId" = "FerrofinLiveTvChannels"."Id" AND p."IsKids" = 1) AS "IsKids"
           FROM "FerrofinLiveTvChannels" ORDER BY "SortIndex", "Name""#,
    )
    .fetch_all(db.pool())
    .await
    .map_err(db_err)
}

/// One channel by id, or `None` when unknown. The aggregated kind flags are
/// not derived here — nothing on the single-channel path reads them (they are
/// `[JsonIgnore]` upstream, filter-only).
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn channel_row(db: &Database, id: Uuid) -> Result<Option<ChannelRow>, ServiceError> {
    sqlx::query_as(
        r#"SELECT "Id","TvgId","Name","Number","ChannelType","DateCreated",
                  0 AS "IsMovie", 0 AS "IsSeries", 0 AS "IsKids"
           FROM "FerrofinLiveTvChannels" WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(id))
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)
}

/// The user's channel user-data rows, keyed by the stored channel id:
/// `(IsFavorite, Rating)`. Backs the favourite/like filters and
/// favourite-first sorting, which upstream pushes into the item repository's
/// SQL.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn channel_user_data(
    db: &Database,
    user_id: Uuid,
) -> Result<ChannelUserData, ServiceError> {
    let rows = sqlx::query(
        r#"SELECT ud."ItemId", ud."IsFavorite", ud."Rating"
           FROM "UserData" ud
           JOIN "FerrofinLiveTvChannels" c ON c."Id" = ud."ItemId"
           WHERE ud."UserId" = ?1 AND ud."CustomDataKey" = lower(ud."ItemId")"#,
    )
    .bind(guid_to_db(user_id))
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|r| {
            (
                r.get::<String, _>("ItemId"),
                (
                    r.get::<bool, _>("IsFavorite"),
                    r.get::<Option<f64>, _>("Rating"),
                ),
            )
        })
        .collect())
}

/// The `Id → DateCreated` map for one tuner's current lineup, read inside the
/// refresh transaction so a re-inserted channel keeps its first-seen instant.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn existing_channel_dates(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    tuner_id: &str,
) -> Result<std::collections::HashMap<String, Option<String>>, ServiceError> {
    Ok(sqlx::query(
        r#"SELECT "Id","DateCreated" FROM "FerrofinLiveTvChannels" WHERE "TunerHostId" = ?1"#,
    )
    .bind(tuner_id)
    .fetch_all(&mut **tx)
    .await
    .map_err(db_err)?
    .into_iter()
    .map(|r| (r.get("Id"), r.get("DateCreated")))
    .collect())
}

/// Whether any tuner host row exists.
///
/// Backs the synchronous `has_tuner_hosts` flag the "Refresh Guide" task's
/// hidden rule polls: a row count rather than a DTO read, so a single
/// undeserializable `Data` blob cannot make a configured tuner vanish.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn tuner_hosts_exist(db: &Database) -> Result<bool, ServiceError> {
    let exists: i64 =
        sqlx::query_scalar(r#"SELECT EXISTS(SELECT 1 FROM "FerrofinLiveTvTunerHosts")"#)
            .fetch_one(db.pool())
            .await
            .map_err(db_err)?;
    Ok(exists != 0)
}

/// The tuner stream URL and owning tuner-host id of one channel, or `None`
/// when the id is not a known channel — what opening a live stream starts from.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn channel_stream_source(
    db: &Database,
    id: Uuid,
) -> Result<Option<(String, String)>, ServiceError> {
    let row = sqlx::query(
        r#"SELECT "StreamUrl","TunerHostId" FROM "FerrofinLiveTvChannels" WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(id))
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    Ok(row.map(|r| {
        (
            r.get::<String, _>("StreamUrl"),
            r.get::<String, _>("TunerHostId"),
        )
    }))
}

/// Maps a `sqlx` error into a [`ServiceError`] via `ferrofin-db`'s `DbError`.
fn db_err(e: sqlx::Error) -> ServiceError {
    ServiceError::from(ferrofin_db::DbError::from(e))
}

/// Seeding helpers for the manager's channel tests (they live here so the raw
/// SQL stays inside the repository boundary).
#[cfg(test)]
#[allow(clippy::missing_errors_doc)] // test seeding helpers; every error is "the write failed"
pub mod test_support {
    use super::{Database, db_err};
    use ferrofin_traits::error::ServiceError;

    /// Inserts the minimal `BaseItems` row `UserData`'s FK needs for an item id.
    pub async fn seed_base_item_stub(db: &Database, id_db: &str) -> Result<(), ServiceError> {
        sqlx::query(
            r#"INSERT INTO "BaseItems"
               ("Id","Type","IsFolder","IsInMixedFolder","IsLocked","IsMovie",
                "IsRepeat","IsSeries","IsVirtualItem")
               VALUES (?1, 'MediaBrowser.Controller.LiveTv.LiveTvChannel', 0,0,0,0,0,0,0)"#,
        )
        .bind(id_db)
        .execute(db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Marks an item favourite for a user, the way the playstate path stores it.
    pub async fn seed_favorite(
        db: &Database,
        item_id_db: &str,
        user_id: &str,
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"INSERT INTO "UserData"
               ("ItemId","UserId","CustomDataKey","IsFavorite","PlayCount",
                "PlaybackPositionTicks","Played")
               VALUES (?1, ?2, lower(?1), 1, 0, 0, 0)"#,
        )
        .bind(item_id_db)
        .bind(user_id)
        .execute(db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Pins every channel's `DateCreated` to a sentinel value.
    pub async fn pin_channel_dates(db: &Database, value: &str) -> Result<(), ServiceError> {
        sqlx::query(r#"UPDATE "FerrofinLiveTvChannels" SET "DateCreated" = ?1"#)
            .bind(value)
            .execute(db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Every channel's stored `DateCreated`.
    pub async fn channel_dates(db: &Database) -> Result<Vec<String>, ServiceError> {
        sqlx::query_scalar(r#"SELECT "DateCreated" FROM "FerrofinLiveTvChannels""#)
            .fetch_all(db.pool())
            .await
            .map_err(db_err)
    }

    /// Every channel's stored id, in lineup order.
    pub async fn channel_ids(db: &Database) -> Result<Vec<String>, ServiceError> {
        sqlx::query_scalar(r#"SELECT "Id" FROM "FerrofinLiveTvChannels" ORDER BY "SortIndex""#)
            .fetch_all(db.pool())
            .await
            .map_err(db_err)
    }
}
