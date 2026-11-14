//! Row reads for the guide's channel projection paths.
//!
//! The SQL boundary keeps raw queries in repository modules; the manager
//! composes these reads with the DTO-service projection in
//! [`crate::manager::FerrofinLiveTvManager`].

use ferrofin_db::Database;
use ferrofin_db::store::guid_to_db;
use sqlx::Row as _;
use uuid::Uuid;

use ferrofin_traits::error::{ServiceError, db_err};
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ParentalRestrictions;
use ferrofin_traits::stubs::LiveTvChannelQuery;

use crate::projection::{ChannelRow, ChannelUserData, ChannelUserDataRow, ProgramRow};

/// A user's parental restrictions for a guide read: the item layer's
/// predicate and the query its user's preferences were loaded onto.
pub(crate) type UserRestrictions<'a> =
    Option<&'a (&'a dyn ParentalRestrictions, InternalItemsQuery)>;

/// Pushes the user's maximum rating, blocked unrated kinds and inherited
/// tags, as `TranslateQuery` applies them to the programme or channel items
/// upstream lists: an `EXISTS` over the `BaseItems` row whose id is
/// `id_column`, a trusted column expression (never a value). A guide row whose
/// item is not stored yet is not shown to such a user.
pub(crate) fn push_item_restrictions(
    qb: &mut sqlx::QueryBuilder<sqlx::Sqlite>,
    id_column: &str,
    (filter, user): &(&dyn ParentalRestrictions, InternalItemsQuery),
) {
    qb.push(format_args!(
        r#"EXISTS (SELECT 1 FROM "BaseItems" bi WHERE bi."Id" = {id_column}"#
    ));
    filter.push_restrictions(qb, user, "bi");
    qb.push(")");
}

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
/// The lineup channels the kind filters of `query` keep, in stored
/// (`SortIndex`) order.
///
/// Upstream's `GetInternalChannels` lists the stored channel items, so its
/// kind filters read what the guide refresh stored on them
/// (`save_channel_aggregates`; `TranslateQuery`, v12.4
/// Jellyfin.Server.Implementations/Item/BaseItemRepository.TranslateQuery.cs:206-258,665-677):
/// `IsMovie` and `IsSeries` the item's columns, `IsKids`, `IsNews` and
/// `IsSports` its "Kids", "News" and "Sports" tags: any of the tags asked for,
/// none of those asked against. The "Kids" tag stays once
/// added, so a channel whose kids airing left the guide still matches.
///
/// With `stored_items_only`, a lineup channel is listed only once its item is
/// stored, as upstream lists only the items (a guide refresh writes the lineup
/// before the items). The guide refresh's own mirror reads the whole lineup.
///
/// `restrictions` are the user's parental restrictions, which keep only the
/// channels whose item that user may list.
///
/// # Errors
///
/// Fails when the database read fails.
pub(crate) async fn channel_rows(
    db: &Database,
    query: &LiveTvChannelQuery,
    restrictions: UserRestrictions<'_>,
    stored_items_only: bool,
) -> Result<Vec<ChannelRow>, ServiceError> {
    channel_rows_query(query, restrictions, stored_items_only)
        .build_query_as()
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// [`channel_rows`]' statement, shared with its plan pin.
fn channel_rows_query(
    query: &LiveTvChannelQuery,
    restrictions: UserRestrictions<'_>,
    stored_items_only: bool,
) -> sqlx::QueryBuilder<sqlx::Sqlite> {
    let mut qb = sqlx::QueryBuilder::new(
        r#"SELECT "Id","TvgId","ExternalId","Name","Number","ChannelType","DateCreated","ImageUrl"
           FROM "FerrofinLiveTvChannels""#,
    );
    let mut joiner = " WHERE ";
    // The restrictions' `EXISTS` over the item lists only stored items too.
    if let Some(restrictions) = restrictions {
        qb.push(std::mem::replace(&mut joiner, " AND "));
        push_item_restrictions(&mut qb, r#""FerrofinLiveTvChannels"."Id""#, restrictions);
    } else if stored_items_only {
        qb.push(std::mem::replace(&mut joiner, " AND "));
        qb.push(
            r#"EXISTS (SELECT 1 FROM "BaseItems" bi WHERE bi."Id" = "FerrofinLiveTvChannels"."Id")"#,
        );
    }
    for (filter, column) in [(query.is_movie, "IsMovie"), (query.is_series, "IsSeries")] {
        if let Some(want) = filter {
            qb.push(std::mem::replace(&mut joiner, " AND "));
            qb.push(format_args!(
                r#"COALESCE((SELECT bi."{column}" FROM "BaseItems" bi
                     WHERE bi."Id" = "FerrofinLiveTvChannels"."Id"), 0) = "#
            ))
            .push_bind(want);
        }
    }
    // Upstream gathers the kinds asked for as one tag list and the kinds asked
    // against as one excluded list: a channel matches when it carries any of
    // the first and none of the second.
    let kinds = [
        (query.is_kids, "Kids"),
        (query.is_news, "News"),
        (query.is_sports, "Sports"),
    ];
    for want in [true, false] {
        let tags: Vec<String> = kinds
            .iter()
            .filter(|(filter, _)| *filter == Some(want))
            .map(|(_, tag)| ferrofin_util::string_extensions::get_clean_value(tag))
            .collect();
        if tags.is_empty() {
            continue;
        }
        qb.push(std::mem::replace(&mut joiner, " AND "));
        if !want {
            qb.push("NOT ");
        }
        qb.push(
            r#"EXISTS (SELECT 1 FROM "ItemValuesMap" m
               JOIN "ItemValues" v ON v."ItemValueId" = m."ItemValueId"
               WHERE m."ItemId" = "FerrofinLiveTvChannels"."Id" AND v."Type" = "#,
        )
        .push_bind(i32::from(ferrofin_db::enums::ItemValueType::Tags))
        .push(r#" AND v."CleanValue" IN ("#);
        let mut separated = qb.separated(", ");
        for tag in tags {
            separated.push_bind(tag);
        }
        qb.push("))");
    }
    qb.push(r#" ORDER BY "SortIndex", "Name""#);
    qb
}

/// One channel by id, or `None` when unknown.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn channel_row(db: &Database, id: Uuid) -> Result<Option<ChannelRow>, ServiceError> {
    sqlx::query_as(
        r#"SELECT "Id","TvgId","ExternalId","Name","Number","ChannelType","DateCreated"
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
    for chunk in ferrofin_db::padded_in_chunks(ids) {
        let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> = sqlx::QueryBuilder::new(
            r#"SELECT "Id","TvgId","ExternalId","Name","Number","ChannelType","DateCreated"
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

/// One channel's stored guide programmes, channel-joined.
///
/// The guide mirror's unit of work. Whatever `insert_programs` left in
/// `FerrofinLiveTvPrograms` after the whole refresh is upstream's
/// `newProgramIdList` across every service, and upstream stores it one channel
/// at a time: `RefreshChannelsInternal` reads each channel's existing
/// programmes (`ChannelIds = [currentChannel.Id]`, v12.4
/// src/Jellyfin.LiveTv/Guide/GuideManager.cs:250-255) and creates or updates
/// them before it moves on. Reading the table one channel at a time keeps the
/// mirror's memory to one channel's airings rather than the whole guide's. The
/// `("ChannelId", "StartDate")` index serves it.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn channel_program_rows(
    db: &Database,
    channel_id: &str,
) -> Result<Vec<ProgramRow>, ServiceError> {
    channel_program_rows_query(channel_id)
        .build_query_as()
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
}

/// [`channel_program_rows`]'s statement, shared with its plan pin.
fn channel_program_rows_query(channel_id: &str) -> sqlx::QueryBuilder<sqlx::Sqlite> {
    let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> = sqlx::QueryBuilder::new(PROGRAM_SELECT);
    qb.push(r#" WHERE p."ChannelId" = "#);
    qb.push_bind(channel_id.to_owned());
    qb
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
    for chunk in ferrofin_db::padded_in_chunks(ids) {
        let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> = sqlx::QueryBuilder::new(PROGRAM_SELECT);
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

/// The user's channel user-data rows, keyed by the stored channel id. Backs
/// the favourite/like filters, favourite-first sorting and the play-state
/// sort keys, which upstream pushes into the item repository's SQL.
///
/// # Errors
///
/// Fails when the database read fails.
pub async fn channel_user_data(
    db: &Database,
    user_id: Uuid,
) -> Result<ChannelUserData, ServiceError> {
    let rows = sqlx::query(
        r#"SELECT ud."ItemId", ud."IsFavorite", ud."Rating", ud."PlayCount", ud."Played",
                  ud."LastPlayedDate"
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
                ChannelUserDataRow {
                    is_favorite: r.get("IsFavorite"),
                    rating: r.get("Rating"),
                    play_count: r.get("PlayCount"),
                    played: r.get("Played"),
                    last_played_date: r.get("LastPlayedDate"),
                },
            )
        })
        .collect())
}

/// One channel of a tuner host's stored lineup, as the guide-binding path
/// needs it.
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
/// Backs the guide refresh's channel binding, which reads it right after the
/// refresh stored the lineup it fetched. The channel-mapping dialog reads each
/// tuner live instead, as upstream's `GetChannelsForListingsProvider` does.
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
/// guide refresh, and the programme mirror's walk over the lineup, one channel
/// at a time.
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

/// The items among `ids` that have a tag in `ItemValues`, which the blocked
/// and allowed tag rules read: the guide refresh asks it of its programmes and
/// of its channels. An item stored before the guide indexed its tags has none,
/// so the refresh writes them once.
///
/// # Errors
///
/// Fails when the database read fails.
pub(crate) async fn programs_with_tag_values(
    db: &Database,
    ids: &[Uuid],
) -> Result<std::collections::HashSet<Uuid>, ServiceError> {
    let mut out = std::collections::HashSet::new();
    for chunk in ferrofin_db::padded_in_chunks(ids) {
        let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> = sqlx::QueryBuilder::new(
            r#"SELECT DISTINCT m."ItemId" FROM "ItemValuesMap" m
               JOIN "ItemValues" v ON v."ItemValueId" = m."ItemValueId"
               WHERE v."Type" = "#,
        );
        qb.push_bind(i32::from(ferrofin_db::enums::ItemValueType::Tags))
            .push(r#" AND m."ItemId" IN ("#);
        let mut separated = qb.separated(", ");
        for id in chunk {
            separated.push_bind(guid_to_db(*id));
        }
        qb.push(")");
        let rows: Vec<String> = qb
            .build_query_scalar()
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
        out.extend(rows.iter().filter_map(|id| Uuid::parse_str(id).ok()));
    }
    Ok(out)
}

/// The ids of the airings stored on one channel, read on `conn`: inside a
/// guide write's transaction, which removes those it did not re-emit.
///
/// # Errors
///
/// Fails when the database read fails.
pub(crate) async fn channel_program_ids(
    conn: &mut sqlx::SqliteConnection,
    channel_id: &str,
) -> Result<Vec<String>, ServiceError> {
    sqlx::query_scalar(r#"SELECT "Id" FROM "FerrofinLiveTvPrograms" WHERE "ChannelId" = ?1"#)
        .bind(channel_id)
        .fetch_all(conn)
        .await
        .map_err(db_err)
}

/// [`delete_programs`] on `conn`: inside a guide write's transaction.
///
/// # Errors
///
/// Fails when the database write fails.
pub(crate) async fn delete_programs_in(
    conn: &mut sqlx::SqliteConnection,
    ids: &[String],
) -> Result<(), ServiceError> {
    for chunk in ids.chunks(IDS_PER_DELETE) {
        delete_by_id_statement("FerrofinLiveTvPrograms", chunk)
            .build()
            .execute(&mut *conn)
            .await
            .map_err(db_err)?;
    }
    Ok(())
}

/// Deletes rows of `table` by id, in bind-limit chunks.
///
/// `table` is one of two crate-private literals, never caller input.
async fn delete_by_id(
    db: &Database,
    table: &'static str,
    ids: &[String],
) -> Result<(), ServiceError> {
    for chunk in ids.chunks(IDS_PER_DELETE) {
        delete_by_id_statement(table, chunk)
            .build()
            .execute(db.writer())
            .await
            .map_err(db_err)?;
    }
    Ok(())
}

/// How many ids one delete-by-id statement binds, under SQLite's
/// 999-parameter limit.
const IDS_PER_DELETE: usize = 500;

/// The statement deleting the rows of `table` whose ids are `ids`.
fn delete_by_id_statement(table: &'static str, ids: &[String]) -> sqlx::QueryBuilder<sqlx::Sqlite> {
    let mut qb: sqlx::QueryBuilder<sqlx::Sqlite> =
        sqlx::QueryBuilder::new(format!(r#"DELETE FROM "{table}" WHERE "Id" IN ("#));
    let mut separated = qb.separated(", ");
    for id in ids {
        separated.push_bind(id);
    }
    qb.push(")");
    qb
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
/// client id that names no existing row is discarded and a fresh one minted,
/// and a matched row is renamed to the posted spelling as it is replaced
/// ([`store_tuner_host`], [`store_listing_provider`]).
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
    sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(wanted)
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)
}

/// Stores a listings provider's row (`Type`, `Path` and the JSON `Data`),
/// first renaming the stored row to `id` when `renamed_from` is the same id
/// spelled differently.
///
/// The configuration half of `ListingsManager.SaveListingProvider`, whose
/// `config.ListingProviders[index] = info` keeps the posted spelling of a
/// matched id. One transaction, so a failed write leaves the old row as it was.
///
/// # Errors
///
/// Fails when the database write fails.
pub async fn store_listing_provider(
    db: &Database,
    renamed_from: Option<&str>,
    id: &str,
    provider_type: &str,
    path: &str,
    data: &str,
) -> Result<(), ServiceError> {
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    if let Some(stored) = renamed_from.filter(|stored| *stored != id) {
        sqlx::query(r#"UPDATE "FerrofinLiveTvListingProviders" SET "Id" = ?1 WHERE "Id" = ?2"#)
            .bind(id)
            .bind(stored)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
    }
    sqlx::query(
        r#"INSERT INTO "FerrofinLiveTvListingProviders" ("Id","Type","Path","Data") VALUES (?1,?2,?3,?4)
           ON CONFLICT("Id") DO UPDATE SET "Type"=excluded."Type","Path"=excluded."Path","Data"=excluded."Data""#,
    )
    .bind(id)
    .bind(provider_type)
    .bind(path)
    .bind(data)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)
}

/// Stores a tuner host's row (`Url`, `Type` and the JSON `Data`), first
/// renaming the stored row, and every channel row that names it, to `id` when
/// `renamed_from` is the same id spelled differently.
///
/// The configuration half of `TunerHostManager.SaveTunerHost`, whose
/// `config.TunerHosts[index] = info` keeps the posted spelling of a matched id.
/// Channel ids do not derive from the tuner id, so a rename moves no channel or
/// its user data. One transaction, so a failed write leaves the old rows as
/// they were. Once committed, the cached answer of whether a tuner exists is
/// dropped ([`Database::invalidate_live_tv_tuner_hosts`]).
///
/// # Errors
///
/// Fails when the database write fails.
pub async fn store_tuner_host(
    db: &Database,
    renamed_from: Option<&str>,
    id: &str,
    url: &str,
    tuner_type: &str,
    data: &str,
) -> Result<(), ServiceError> {
    let mut tx = db.writer().begin().await.map_err(db_err)?;
    if let Some(stored) = renamed_from.filter(|stored| *stored != id) {
        sqlx::query(r#"UPDATE "FerrofinLiveTvTunerHosts" SET "Id" = ?1 WHERE "Id" = ?2"#)
            .bind(id)
            .bind(stored)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        sqlx::query(
            r#"UPDATE "FerrofinLiveTvChannels" SET "TunerHostId" = ?1 WHERE "TunerHostId" = ?2"#,
        )
        .bind(id)
        .bind(stored)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
    }
    sqlx::query(
        r#"INSERT INTO "FerrofinLiveTvTunerHosts" ("Id","Url","Type","Data") VALUES (?1,?2,?3,?4)
           ON CONFLICT("Id") DO UPDATE SET "Url"=excluded."Url","Type"=excluded."Type","Data"=excluded."Data""#,
    )
    .bind(id)
    .bind(url)
    .bind(tuner_type)
    .bind(data)
    .execute(&mut *tx)
    .await
    .map_err(db_err)?;
    tx.commit().await.map_err(db_err)?;
    db.invalidate_live_tv_tuner_hosts();
    Ok(())
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

/// Seeding helpers for the manager's channel tests (they live here so the raw
/// SQL stays inside the repository boundary).
#[cfg(test)]
#[allow(clippy::missing_errors_doc)] // test seeding helpers; every error is "the write failed"
pub mod test_support {
    use super::{Database, PROGRAM_SELECT, ProgramRow, db_err};
    use ferrofin_traits::error::ServiceError;

    /// From now on, counts each programme row written to `BaseItems` (an
    /// insert, or an upsert that updates the stored row) and each
    /// `ItemValuesMap` link written or removed. Call it once per database.
    pub async fn count_program_writes(db: &Database) -> Result<(), ServiceError> {
        for statement in [
            r#"CREATE TABLE "TestProgramWrites" ("Kind" TEXT, "Id" TEXT)"#,
            r#"CREATE TRIGGER "TestProgramInserted" AFTER INSERT ON "BaseItems"
               WHEN NEW."Type" = 'MediaBrowser.Controller.LiveTv.LiveTvProgram'
               BEGIN INSERT INTO "TestProgramWrites" VALUES ('item', NEW."Id"); END"#,
            r#"CREATE TRIGGER "TestProgramUpdated" AFTER UPDATE ON "BaseItems"
               WHEN NEW."Type" = 'MediaBrowser.Controller.LiveTv.LiveTvProgram'
               BEGIN INSERT INTO "TestProgramWrites" VALUES ('item', NEW."Id"); END"#,
            r#"CREATE TRIGGER "TestValueLinked" AFTER INSERT ON "ItemValuesMap"
               BEGIN INSERT INTO "TestProgramWrites" VALUES ('value', NEW."ItemId"); END"#,
            r#"CREATE TRIGGER "TestValueUnlinked" AFTER DELETE ON "ItemValuesMap"
               BEGIN INSERT INTO "TestProgramWrites" VALUES ('value', OLD."ItemId"); END"#,
        ] {
            sqlx::query(statement)
                .execute(db.writer())
                .await
                .map_err(db_err)?;
        }
        Ok(())
    }

    /// The writes [`count_program_writes`] has counted since it started, or
    /// since the last call: `(programme rows, value links)`.
    pub async fn take_program_writes(db: &Database) -> Result<(i64, i64), ServiceError> {
        let counts = sqlx::query_as(
            r#"SELECT COALESCE(SUM("Kind" = 'item'), 0), COALESCE(SUM("Kind" = 'value'), 0)
               FROM "TestProgramWrites""#,
        )
        .fetch_one(db.pool())
        .await
        .map_err(db_err)?;
        sqlx::query(r#"DELETE FROM "TestProgramWrites""#)
            .execute(db.writer())
            .await
            .map_err(db_err)?;
        Ok(counts)
    }

    /// Every stored guide programme, channel-joined, in one read.
    pub async fn program_rows(db: &Database) -> Result<Vec<ProgramRow>, ServiceError> {
        sqlx::query_as(PROGRAM_SELECT)
            .fetch_all(db.pool())
            .await
            .map_err(db_err)
    }

    /// The `EXPLAIN QUERY PLAN` details of [`super::channel_program_rows`]'s
    /// statement, in plan order.
    pub async fn channel_program_rows_plan(
        db: &Database,
        channel_id: &str,
    ) -> Result<Vec<String>, ServiceError> {
        plan(db, super::channel_program_rows_query(channel_id)).await
    }

    /// The `EXPLAIN QUERY PLAN` details of [`super::channel_rows`]' statement
    /// for `query`, `restrictions` and `stored_items_only`, in plan order.
    pub(crate) async fn channel_rows_plan(
        db: &Database,
        query: &super::LiveTvChannelQuery,
        restrictions: super::UserRestrictions<'_>,
        stored_items_only: bool,
    ) -> Result<Vec<String>, ServiceError> {
        plan(
            db,
            super::channel_rows_query(query, restrictions, stored_items_only),
        )
        .await
    }

    /// The `EXPLAIN QUERY PLAN` details of `qb`'s statement, in plan order.
    pub async fn plan(
        db: &Database,
        mut qb: sqlx::QueryBuilder<sqlx::Sqlite>,
    ) -> Result<Vec<String>, ServiceError> {
        use sqlx::Execute as _;
        let mut query = qb.build();
        let args = query
            .take_arguments()
            .map_err(|e| ServiceError::Backend(e.to_string()))?
            .unwrap_or_default();
        let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .map(|row| sqlx::Row::get::<String, _>(row, "detail"))
            .collect())
    }

    /// Overwrites a configuration row's `Data` with JSON that no longer parses;
    /// `table` is [`super::TUNER_HOSTS_TABLE`] or
    /// [`super::LISTING_PROVIDERS_TABLE`].
    pub async fn corrupt_config_data(
        db: &Database,
        table: &'static str,
        id: &str,
    ) -> Result<(), ServiceError> {
        let sql = format!(r#"UPDATE "{table}" SET "Data" = '{{' WHERE "Id" = ?1"#);
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id)
            .execute(db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

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

    /// Every programme item's genre links as `(Id, genre)`, ordered — the
    /// `ItemValues` the guide refresh's mirror wrote for its airings.
    pub async fn program_genre_links(db: &Database) -> Result<Vec<(String, String)>, ServiceError> {
        sqlx::query_as(
            r#"SELECT bi."Id", v."Value" FROM "ItemValuesMap" m
               JOIN "ItemValues" v ON v."ItemValueId" = m."ItemValueId"
               JOIN "BaseItems" bi ON bi."Id" = m."ItemId"
               WHERE bi."Type" = ?1 AND v."Type" = 2
               ORDER BY bi."Id", v."Value""#,
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

    /// The item holding `user_id`'s favourite row keyed `key`, if any: the
    /// channel itself, or the deletion placeholder a removed channel's rows
    /// are parked on.
    pub async fn favorite_holder(
        db: &Database,
        user_id: &str,
        key: &str,
    ) -> Result<Option<String>, ServiceError> {
        sqlx::query_scalar(
            r#"SELECT "ItemId" FROM "UserData"
               WHERE "UserId" = ?1 AND "CustomDataKey" = ?2 AND "IsFavorite" = 1"#,
        )
        .bind(user_id)
        .bind(key)
        .fetch_optional(db.pool())
        .await
        .map_err(db_err)
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

    /// Pins every airing's `DateCreated` to a sentinel value, or to NULL (an
    /// airing stored before the column existed).
    pub async fn pin_program_dates(db: &Database, value: Option<&str>) -> Result<(), ServiceError> {
        sqlx::query(r#"UPDATE "FerrofinLiveTvPrograms" SET "DateCreated" = ?1"#)
            .bind(value)
            .execute(db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Every stored airing as `(title, its channel's tvg-id, DateCreated)`,
    /// by title.
    pub async fn program_dates(
        db: &Database,
    ) -> Result<Vec<(String, String, Option<String>)>, ServiceError> {
        sqlx::query_as(
            r#"SELECT p."Title", c."TvgId", p."DateCreated"
               FROM "FerrofinLiveTvPrograms" p
               JOIN "FerrofinLiveTvChannels" c ON c."Id" = p."ChannelId"
               ORDER BY p."Title""#,
        )
        .fetch_all(db.pool())
        .await
        .map_err(db_err)
    }

    /// The rows `conn` has inserted, updated or deleted since it opened
    /// (`total_changes()`), so a test can tell what a statement wrote.
    pub async fn total_changes(conn: &mut sqlx::SqliteConnection) -> Result<i64, ServiceError> {
        sqlx::query_scalar("SELECT total_changes()")
            .fetch_one(conn)
            .await
            .map_err(db_err)
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
