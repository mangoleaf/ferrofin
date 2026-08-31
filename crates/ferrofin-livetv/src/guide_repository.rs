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

use crate::projection::{ChannelRow, ChannelUserData, ProgramRow};

/// The columns a guide list read returns, joined to the owning channel for
/// `ChannelName`. Held apart from the filters so the `WHERE`/`ORDER`/`LIMIT`
/// builders can be shared with the total-record count.
pub(crate) const PROGRAM_SELECT: &str = r#"SELECT p."Id",p."ChannelId",p."StartDate",p."EndDate",p."Title",p."EpisodeTitle",
                      p."Overview",p."Genres",p."ProductionYear",p."OfficialRating",p."IsNew",
                      p."IsRepeat",p."IsPremiere",p."IsMovie",p."IsSeries",p."IsNews",p."IsKids",
                      p."IsSports",p."IsLive",p."ExternalId",p."ExternalSeriesId",
                      p."SeasonNumber",p."EpisodeNumber",p."DateCreated",p."ShowId",
                      c."Name" AS "ChannelName",c."Number" AS "ChannelNumber",
                      c."ChannelType" AS "ChannelMediaKind"
               FROM "FerrofinLiveTvPrograms" p
               JOIN "FerrofinLiveTvChannels" c ON c."Id" = p."ChannelId""#;
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
        r#"SELECT "Id","TvgId","ExternalId","Name","Number","ChannelType","DateCreated",
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
        r#"SELECT "Id","TvgId","ExternalId","Name","Number","ChannelType","DateCreated",
                  0 AS "IsMovie", 0 AS "IsSeries", 0 AS "IsKids"
           FROM "FerrofinLiveTvChannels" WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(id))
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)
}

/// The channels among `ids`, keyed by their `Uuid`.
///
/// One query for a whole page: `AddChannelInfo` runs over every DTO the
/// projection produced, and a per-DTO lookup would be an N+1 on the channel
/// list. Ids that are not channels are simply absent from the map, which is
/// how a mixed page (an ordinary `/Items` response) costs one query and
/// changes nothing.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn channel_rows_by_ids(
    db: &Database,
    ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, ChannelRow>, ServiceError> {
    let mut out = std::collections::HashMap::new();
    // One bind per id, so the page has to be chunked: a recursive `/Items` page
    // can name thousands of items and a channel only has to be one of them for
    // this lookup to run.
    for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let mut qb: sqlx::QueryBuilder<'_, sqlx::Sqlite> = sqlx::QueryBuilder::new(
            r#"SELECT "Id","TvgId","ExternalId","Name","Number","ChannelType","DateCreated",
                  0 AS "IsMovie", 0 AS "IsSeries", 0 AS "IsKids"
           FROM "FerrofinLiveTvChannels" WHERE "Id" IN ("#,
        );
        let mut separated = qb.separated(", ");
        for id in chunk {
            separated.push_bind(guid_to_db(*id));
        }
        qb.push(")");
        let rows: Vec<ChannelRow> = qb
            .build_query_as()
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
        out.extend(
            rows.into_iter()
                .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row))),
        );
    }
    Ok(out)
}

/// Every stored guide programme, channel-joined.
///
/// The mirror's source of truth: whatever `insert_programs` left in
/// `FerrofinLiveTvPrograms` after the whole refresh is exactly upstream's
/// `newProgramIdList` accumulated across every service.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn program_rows(db: &Database) -> Result<Vec<ProgramRow>, ServiceError> {
    sqlx::query_as(PROGRAM_SELECT)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// The guide programmes among `ids`, keyed by their `Uuid`.
///
/// One query for a whole page, for the same reason [`channel_rows_by_ids`] is:
/// the `DtoService` seam runs over every DTO on an `/Items` page and a per-DTO
/// lookup would be an N+1. Ids that are not programmes are simply absent from
/// the map, which is how a mixed page costs one query and changes nothing.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn program_rows_by_ids(
    db: &Database,
    ids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, ProgramRow>, ServiceError> {
    let mut out = std::collections::HashMap::new();
    for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let mut qb: sqlx::QueryBuilder<'_, sqlx::Sqlite> = sqlx::QueryBuilder::new(PROGRAM_SELECT);
        qb.push(r#" WHERE p."Id" IN ("#);
        let mut separated = qb.separated(", ");
        for id in chunk {
            separated.push_bind(guid_to_db(*id));
        }
        qb.push(")");
        let rows: Vec<ProgramRow> = qb
            .build_query_as()
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
        out.extend(
            rows.into_iter()
                .filter_map(|row| Uuid::parse_str(&row.id).ok().map(|id| (id, row))),
        );
    }
    Ok(out)
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

/// One channel of a tuner host's stored lineup, as the channel-mapping and
/// guide-binding paths need it.
///
/// The stored [`id`](Self::id) is Ferrofin's internal channel key;
/// [`external_id`](Self::external_id) is the `ChannelInfo.Id` the owning
/// [`TunerHost`](crate::tuner_host::TunerHost) minted — `m3u_{md5}{md5}` for a
/// playlist, `hdhr_{GuideNumber}` for an HDHomeRun. It is READ, never
/// re-derived here: re-deriving it in the M3U shape would mislabel every
/// HDHomeRun channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunerLineupRow {
    /// The stored channel id (Ferrofin's internal key).
    pub id: String,
    /// The playlist's `tvg-id` — `ChannelInfo.TunerChannelId`.
    pub tvg_id: String,
    /// The display name.
    pub name: String,
    /// The channel number, or empty when the playlist carried none.
    pub number: String,
    /// The stream URL the channel plays from.
    pub stream_url: String,
    /// The tuner host's `ChannelInfo.Id` for this channel.
    pub external_id: String,
}

/// The user's channel user-data rows keyed by the stored channel id, in the
/// shape `LiveTvManager.GetRecommendationScore` reads:
/// `(Likes, IsFavorite, PlayCount)`.
///
/// Kept apart from [`channel_user_data`] because the recommendation score wants
/// `Likes`/`PlayCount` and the filters want `Rating`; one query returning both
/// shapes would make every favourite-filtered guide read pay for columns it
/// throws away.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn channel_recommendation_data(
    db: &Database,
    user_id: Uuid,
) -> Result<std::collections::HashMap<String, (Option<bool>, bool, i32)>, ServiceError> {
    let rows = sqlx::query(
        r#"SELECT ud."ItemId", ud."Likes", ud."IsFavorite", ud."PlayCount"
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
                    r.get::<Option<bool>, _>("Likes"),
                    r.get::<bool, _>("IsFavorite"),
                    r.get::<i32, _>("PlayCount"),
                ),
            )
        })
        .collect())
}

/// One tuner host's stored lineup, in playlist order.
///
/// Backs the port of `ListingsManager.GetChannelsForListingsProvider`, which
/// upstream re-parses the tuner's M3U on every call; Ferrofin already caches
/// the parse in this table, so the same `ChannelInfo` fields are read back
/// from it.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn tuner_lineup(
    db: &Database,
    tuner_host_id: &str,
) -> Result<Vec<TunerLineupRow>, ServiceError> {
    let rows = sqlx::query(
        r#"SELECT "Id","TvgId","Name","Number","StreamUrl","ExternalId" FROM "FerrofinLiveTvChannels"
           WHERE "TunerHostId" = ?1 COLLATE NOCASE ORDER BY "SortIndex", "Name""#,
    )
    .bind(tuner_host_id)
    .fetch_all(db.pool())
    .await
    .map_err(db_err)?;
    Ok(rows
        .into_iter()
        .map(|r| TunerLineupRow {
            id: r.get("Id"),
            tvg_id: r.get("TvgId"),
            name: r.get("Name"),
            number: r.get::<Option<String>, _>("Number").unwrap_or_default(),
            stream_url: r.get("StreamUrl"),
            external_id: r.get("ExternalId"),
        })
        .collect())
}

/// Every channel id currently cached.
///
/// Backs the `CleanDatabase(newChannelIdList, [LiveTvChannel], …)` half of the
/// guide refresh.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn all_channel_ids(db: &Database) -> Result<Vec<String>, ServiceError> {
    sqlx::query_scalar(r#"SELECT "Id" FROM "FerrofinLiveTvChannels""#)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// Deletes the channels whose ids are listed (their airings go with them,
/// through the programme table's foreign key).
///
/// # Errors
///
/// Fails when the database write fails.
pub async fn delete_channels(db: &Database, ids: &[String]) -> Result<(), ServiceError> {
    delete_by_id(db, "FerrofinLiveTvChannels", ids).await
}

/// Every programme id currently cached.
///
/// Backs the port of `GuideManager.CleanDatabase`, which lists every stored
/// `LiveTvProgram` and deletes the ones the refresh pass did not re-emit.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn all_program_ids(db: &Database) -> Result<Vec<String>, ServiceError> {
    sqlx::query_scalar(r#"SELECT "Id" FROM "FerrofinLiveTvPrograms""#)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// Deletes the programmes whose ids are listed.
///
/// The second half of the [`all_program_ids`] pair: `CleanDatabase` deletes the
/// stale rows one item at a time upstream; here they go in bind-limit chunks.
///
/// # Errors
///
/// Fails when the database write fails.
pub async fn delete_programs(db: &Database, ids: &[String]) -> Result<(), ServiceError> {
    delete_by_id(db, "FerrofinLiveTvPrograms", ids).await
}

/// Deletes rows of `table` by id, in bind-limit chunks.
///
/// `table` is one of two crate-private literals, never caller input.
async fn delete_by_id(
    db: &Database,
    table: &'static str,
    ids: &[String],
) -> Result<(), ServiceError> {
    for chunk in ids.chunks(500) {
        let mut qb: sqlx::QueryBuilder<'_, sqlx::Sqlite> =
            sqlx::QueryBuilder::new(format!(r#"DELETE FROM "{table}" WHERE "Id" IN ("#));
        let mut separated = qb.separated(", ");
        for id in chunk {
            separated.push_bind(id);
        }
        qb.push(")");
        qb.build().execute(db.writer()).await.map_err(db_err)?;
    }
    Ok(())
}

/// The tuner-host configuration table.
pub const TUNER_HOSTS_TABLE: &str = "FerrofinLiveTvTunerHosts";
/// The listings-provider configuration table.
pub const LISTING_PROVIDERS_TABLE: &str = "FerrofinLiveTvListingProviders";

/// The stored spelling of a configuration row's id, when one matches `wanted`
/// case-insensitively.
///
/// Backs the `Array.FindIndex(..., OrdinalIgnoreCase)` half of
/// `TunerHostManager.SaveTunerHost` / `ListingsManager.SaveListingProvider`: a
/// client id that names no existing row is discarded and a fresh one minted.
/// The stored spelling is what comes back, because the channel rows reference
/// their tuner host by that exact string.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn existing_config_id(
    db: &Database,
    table: &'static str,
    wanted: &str,
) -> Result<Option<String>, ServiceError> {
    // `table` is one of the two crate constants above, never caller input.
    let sql = format!(r#"SELECT "Id" FROM "{table}" WHERE "Id" = ?1 COLLATE NOCASE"#);
    sqlx::query_scalar(&sql)
        .bind(wanted)
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)
}

/// The tuner-facing external id of one channel, or `None` when the lineup does
/// not hold it (or holds it empty, as a row written before migration 0023).
///
/// This is `SeriesTimerInfo.ChannelId`/`TimerInfo.ChannelId` upstream — the id
/// the internal channel GUID was derived from, published on every timer DTO as
/// `ExternalChannelId` (`LiveTvDtoService.cs:69`, `:137`).
///
/// # Errors
///
/// Fails when the read fails.
pub async fn channel_external_id(
    db: &Database,
    channel_id: Uuid,
) -> Result<Option<String>, ServiceError> {
    let external: Option<String> =
        sqlx::query_scalar(r#"SELECT "ExternalId" FROM "FerrofinLiveTvChannels" WHERE "Id" = ?1"#)
            .bind(guid_to_db(channel_id))
            .fetch_optional(db.pool())
            .await
            .map_err(db_err)?;
    Ok(external.filter(|e| !e.is_empty()))
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
) -> Result<Option<(crate::tuner_host::StoredChannel, String)>, ServiceError> {
    let row = sqlx::query(
        r#"SELECT "StreamUrl","TunerHostId","ExternalId","IsHd","VideoCodec","AudioCodec"
           FROM "FerrofinLiveTvChannels" WHERE "Id" = ?1"#,
    )
    .bind(guid_to_db(id))
    .fetch_optional(db.pool())
    .await
    .map_err(db_err)?;
    Ok(row.map(|r| {
        (
            crate::tuner_host::StoredChannel {
                external_id: r.get::<String, _>("ExternalId"),
                path: r.get::<String, _>("StreamUrl"),
                // `ChannelInfo.IsHD` is `bool?`: NULL is "the tuner did not
                // say", which `HdHomerunHost.GetMediaSource` reads as `?? true`.
                is_hd: r.get::<Option<i64>, _>("IsHd").map(|v| v != 0),
                video_codec: r.get::<Option<String>, _>("VideoCodec"),
                audio_codec: r.get::<Option<String>, _>("AudioCodec"),
            },
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

    /// The stored `BaseItems` channel-item rows as
    /// `(Id, ParentId, TopParentId)`, id-ordered — what the guide refresh's
    /// `BaseItems` mirror wrote.
    pub async fn stored_channel_items(
        db: &Database,
    ) -> Result<Vec<(String, Option<String>, Option<String>)>, ServiceError> {
        sqlx::query_as(
            r#"SELECT "Id","ParentId","TopParentId" FROM "BaseItems"
               WHERE "Type" = ?1 ORDER BY "Id""#,
        )
        .bind(crate::projection::CHANNEL_TYPE_NAME)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
    }

    /// The stored `BaseItems` programme-item rows as
    /// `(Id, ParentId, TopParentId, PresentationUniqueKey)`, id-ordered — what
    /// the guide refresh's `BaseItems` mirror wrote.
    ///
    /// # Errors
    ///
    /// Fails when the database read fails.
    pub async fn stored_program_items(
        db: &Database,
    ) -> Result<Vec<(String, Option<String>, Option<String>, Option<String>)>, ServiceError> {
        sqlx::query_as(
            r#"SELECT "Id","ParentId","TopParentId","PresentationUniqueKey" FROM "BaseItems"
               WHERE "Type" = ?1 ORDER BY "Id""#,
        )
        .bind(crate::projection::PROGRAM_TYPE_NAME)
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
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
