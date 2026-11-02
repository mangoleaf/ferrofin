//! The "latest" items of a TV library — v12 `BaseItemRepository.
//! GetLatestTvShowItems` (`Jellyfin.Server.Implementations/Item/
//! BaseItemRepository.Querying.cs:282-540`).
//!
//! For the newest `limit` series it returns ONE row each, the card that best
//! stands for what was just added, from the episodes added within 24 hours of
//! that series' newest one:
//!
//! - one recent episode → the **Episode**;
//! - several in one season of a multi-season series (or the whole season) →
//!   the **Season**;
//! - several in the only season → the **Series**;
//! - recent episodes across seasons → the **Series**.
//!
//! A Season or Series the user's parental settings hide is replaced by the
//! episode. The rows come back newest first (`MaxDate DESC, Id DESC`).

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::guid_to_db;
use ferrofin_model::data::BaseItemKind;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use sqlx::{QueryBuilder, Sqlite};
use uuid::Uuid;

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::{
    append_parental_restrictions, build_latest_tv_episodes_query, build_latest_tv_series_query,
    has_parental_restrictions,
};

/// Episodes added within this many hours of a series' newest one count as
/// "recently added together" (upstream's `RecentAdditionWindowHours`, a
/// constant there too).
const RECENT_ADDITION_WINDOW_HOURS: i64 = 24;

/// One row of the step-2 projection.
#[derive(Debug, Clone, sqlx::FromRow)]
#[sqlx(rename_all = "PascalCase")]
struct EpisodeRow {
    id: String,
    series_name: Option<String>,
    date_created: Option<DateTime<Utc>>,
    season_id: Option<String>,
    series_id: Option<String>,
}

/// An episode of the step-2 projection, ids parsed.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct LatestEpisode {
    /// `Id`.
    pub id: Uuid,
    /// `SeriesName`, the grouping key.
    pub series_name: String,
    /// `DateCreated`.
    pub date_created: Option<DateTime<Utc>>,
    /// `SeasonId`.
    pub season_id: Option<Uuid>,
    /// `SeriesId`.
    pub series_id: Option<Uuid>,
}

/// What step 3 learns of one series' recent additions.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SeriesAnalysis {
    /// Episodes within the window of the newest one.
    pub recent_episode_count: usize,
    /// The distinct seasons those episodes are in.
    pub season_ids: Vec<Uuid>,
    /// The first `SeriesId` among them.
    pub first_recent_series_id: Option<Uuid>,
    /// The newest episode's `DateCreated` (the minimum date when it has none).
    pub max_date: DateTime<Utc>,
    /// The newest episode.
    pub most_recent_episode_id: Uuid,
}

/// The card chosen for one series (step 5): a Season, else a Series, else the
/// newest episode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LatestCard {
    /// The Season to show, if any.
    pub season_id: Option<Uuid>,
    /// The Series to show, if any.
    pub series_id: Option<Uuid>,
    /// The series' newest addition, which orders the cards.
    pub max_date: DateTime<Utc>,
    /// The newest episode, shown when neither container is.
    pub most_recent_episode_id: Uuid,
}

/// Step 3 (`Querying.cs:339-378`): `episodes` newest first, grouped by
/// `SeriesName` in the order each series first appears.
pub(crate) fn analyse_series(episodes: &[LatestEpisode]) -> Vec<SeriesAnalysis> {
    let mut groups: Vec<(&str, Vec<&LatestEpisode>)> = Vec::new();
    let mut slot: HashMap<&str, usize> = HashMap::new();
    for episode in episodes {
        let name = episode.series_name.as_str();
        if let Some(&i) = slot.get(name) {
            groups[i].1.push(episode);
        } else {
            slot.insert(name, groups.len());
            groups.push((name, vec![episode]));
        }
    }
    groups
        .into_iter()
        .map(|(_, group)| {
            let newest = group[0];
            let max_date = newest.date_created.unwrap_or(DateTime::<Utc>::MIN_UTC);
            let cutoff = max_date
                .checked_sub_signed(Duration::hours(RECENT_ADDITION_WINDOW_HOURS))
                .unwrap_or(DateTime::<Utc>::MIN_UTC);
            let mut recent_episode_count = 0;
            let mut seasons: Vec<Uuid> = Vec::new();
            let mut first_recent_series_id = None;
            for episode in group
                .iter()
                .filter(|e| e.date_created.is_some_and(|d| d >= cutoff))
            {
                recent_episode_count += 1;
                if let Some(season) = episode.season_id
                    && !seasons.contains(&season)
                {
                    seasons.push(season);
                }
                first_recent_series_id = first_recent_series_id.or(episode.series_id);
            }
            SeriesAnalysis {
                recent_episode_count,
                season_ids: seasons,
                first_recent_series_id,
                max_date,
                most_recent_episode_id: newest.id,
            }
        })
        .collect()
}

/// Step 5 (`Querying.cs:402-456`): the card for each analysed series, from
/// how many episodes each recent season holds and how many seasons each
/// series has (a series with no count is taken to have one season).
pub(crate) fn choose_cards(
    analysis: &[SeriesAnalysis],
    season_episode_counts: &HashMap<Uuid, i64>,
    series_season_counts: &HashMap<Uuid, i64>,
) -> Vec<LatestCard> {
    analysis
        .iter()
        .map(|series| {
            let mut card = LatestCard {
                season_id: None,
                series_id: None,
                max_date: series.max_date,
                most_recent_episode_id: series.most_recent_episode_id,
            };
            match series.season_ids.as_slice() {
                [season] => {
                    let total_episodes = season_episode_counts.get(season).copied().unwrap_or(0);
                    let seasons_in_series = series
                        .first_recent_series_id
                        .map_or(1, |id| series_season_counts.get(&id).copied().unwrap_or(1));
                    let recent = i64::try_from(series.recent_episode_count).unwrap_or(i64::MAX);
                    let multiple_or_all = recent > 1 || recent == total_episodes;
                    if seasons_in_series > 1 && multiple_or_all {
                        card.season_id = Some(*season);
                    } else if multiple_or_all {
                        card.series_id = series.first_recent_series_id;
                    }
                }
                [_, _, ..] => card.series_id = series.first_recent_series_id,
                [] => {}
            }
            card
        })
        .collect()
}

/// v12 `GetLatestTvShowItems` over the caller's `filter`: the newest
/// `filter.limit` series, one row each — the Episode, Season or Series card
/// described in the module docs — newest first.
///
/// # Errors
///
/// [`ServiceError`] on a storage failure.
pub(crate) async fn latest_tv_show_items(
    db: &Database,
    filter: &InternalItemsQuery,
) -> Result<Vec<BaseItemEntity>, ServiceError> {
    // Step 1: the newest series, and the global cutoff before which no
    // episode can be in any of their windows.
    let top: Vec<(String, Option<DateTime<Utc>>)> = build_latest_tv_series_query(filter)
        .build_query_as()
        .fetch_all(db.pool())
        .await
        .map_err(db_err)?;
    if top.is_empty() {
        return Ok(Vec::new());
    }
    let cutoff = top
        .iter()
        .filter_map(|(_, max)| *max)
        .min()
        .and_then(|oldest| {
            oldest.checked_sub_signed(Duration::hours(RECENT_ADDITION_WINDOW_HOURS))
        });
    let names: Vec<String> = top.into_iter().map(|(name, _)| name).collect();

    // Step 2: their episodes, the lightweight projection.
    let episodes: Vec<LatestEpisode> = build_latest_tv_episodes_query(filter, &names, cutoff)
        .build_query_as::<EpisodeRow>()
        .fetch_all(db.pool())
        .await
        .map_err(db_err)?
        .into_iter()
        .filter_map(|row| {
            Some(LatestEpisode {
                id: Uuid::parse_str(&row.id).ok()?,
                series_name: row.series_name?,
                date_created: row.date_created,
                season_id: row.season_id.and_then(|id| Uuid::parse_str(&id).ok()),
                series_id: row.series_id.and_then(|id| Uuid::parse_str(&id).ok()),
            })
        })
        .collect();

    // Steps 3-5: analyse each series and pick its card.
    let analysis = analyse_series(&episodes);
    let mut season_ids: Vec<Uuid> = analysis
        .iter()
        .flat_map(|s| s.season_ids.iter().copied())
        .collect();
    season_ids.sort_unstable();
    season_ids.dedup();
    let mut series_ids: Vec<Uuid> = analysis
        .iter()
        .filter_map(|s| s.first_recent_series_id)
        .collect();
    series_ids.sort_unstable();
    series_ids.dedup();
    let season_episode_counts =
        count_children(db, "SeasonId", BaseItemKind::Episode, &season_ids).await?;
    let series_season_counts =
        count_children(db, "SeriesId", BaseItemKind::Season, &series_ids).await?;
    let mut cards = choose_cards(&analysis, &season_episode_counts, &series_season_counts);

    // Step 5b: a container is what gets returned, so it has to pass the
    // user's parental settings on its own; one that does not is replaced by
    // its episode.
    let containers: Vec<Uuid> = cards
        .iter()
        .filter_map(|card| card.season_id.or(card.series_id))
        .collect();
    if has_parental_restrictions(filter) && !containers.is_empty() {
        let allowed = allowed_ids(db, filter, &containers).await?;
        for card in &mut cards {
            card.season_id = card.season_id.filter(|id| allowed.contains(id));
            card.series_id = card.series_id.filter(|id| allowed.contains(id));
        }
    }

    // Steps 6-7: read the chosen rows and order them.
    let wanted: Vec<Uuid> = cards
        .iter()
        .map(|card| {
            card.season_id
                .or(card.series_id)
                .unwrap_or(card.most_recent_episode_id)
        })
        .collect();
    let rows: HashMap<Uuid, BaseItemEntity> =
        crate::item_repository::select_item_rows(db, r#""Id""#, &wanted)
            .await?
            .into_iter()
            .filter_map(|row| Some((Uuid::parse_str(&row.id).ok()?, row)))
            .collect();
    // Two series groups (two `SeriesName`s) can land on one row; upstream's
    // `TryGetValue` returns it for each.
    let mut results: Vec<(DateTime<Utc>, Uuid, BaseItemEntity)> = cards
        .iter()
        .zip(wanted)
        .filter_map(|(card, id)| Some((card.max_date, id, rows.get(&id)?.clone())))
        .collect();
    results.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    if let Some(limit) = filter.limit {
        results.truncate(usize::try_from(limit).unwrap_or(0));
    }
    Ok(results.into_iter().map(|(_, _, row)| row).collect())
}

/// How many `kind` rows name each of `ids` in `column` (`SeasonId` or
/// `SeriesId`), over every such row, as upstream's step-4 counts are. The
/// `+` keeps the planner on the `column` index rather than a `Type`-leading
/// one.
async fn count_children(
    db: &Database,
    column: &str,
    kind: BaseItemKind,
    ids: &[Uuid],
) -> Result<HashMap<Uuid, i64>, ServiceError> {
    let mut counts = HashMap::new();
    for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let rows: Vec<(String, i64)> = count_children_query(column, kind, chunk)
            .build_query_as()
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
        counts.extend(
            rows.into_iter()
                .filter_map(|(id, count)| Some((Uuid::parse_str(&id).ok()?, count))),
        );
    }
    Ok(counts)
}

/// The [`count_children`] statement for one chunk of `ids`.
fn count_children_query(column: &str, kind: BaseItemKind, ids: &[Uuid]) -> QueryBuilder<Sqlite> {
    let mut qb: QueryBuilder<Sqlite> = QueryBuilder::new(format!(
        r#"SELECT b."{column}", COUNT(*) FROM "BaseItems" AS b WHERE +b."Type" = "#
    ));
    qb.push_bind(stored_type_name(kind).unwrap_or_default());
    qb.push(format!(r#" AND b."{column}" IN ("#));
    let mut list = qb.separated(", ");
    for id in ids {
        list.push_bind(guid_to_db(*id));
    }
    qb.push(format!(r#") GROUP BY b."{column}""#));
    qb
}

/// Of `ids`, the ones `filter`'s parental restrictions let through
/// (`ApplyParentalRestrictions` over the containers, `Querying.cs:461-468`).
async fn allowed_ids(
    db: &Database,
    filter: &InternalItemsQuery,
    ids: &[Uuid],
) -> Result<HashSet<Uuid>, ServiceError> {
    let mut allowed = HashSet::new();
    for chunk in ids.chunks(ferrofin_db::BATCH_BIND_CHUNK) {
        let mut qb: QueryBuilder<Sqlite> =
            QueryBuilder::new(r#"SELECT bi."Id" FROM "BaseItems" AS bi WHERE bi."Id" IN ("#);
        let mut list = qb.separated(", ");
        for id in chunk {
            list.push_bind(guid_to_db(*id));
        }
        qb.push(")");
        append_parental_restrictions(&mut qb, filter, "bi");
        let rows: Vec<(String,)> = qb
            .build_query_as()
            .fetch_all(db.pool())
            .await
            .map_err(db_err)?;
        allowed.extend(
            rows.into_iter()
                .filter_map(|(id,)| Uuid::parse_str(&id).ok()),
        );
    }
    Ok(allowed)
}

/// `EXPLAIN QUERY PLAN` of `qb`'s statement with its binds, one line per
/// step joined by ` | ` — for the plan pins in the tests, kept here so the
/// SQL stays inside the repository boundary.
#[cfg(test)]
pub(crate) async fn explain(db: &Database, mut qb: QueryBuilder<Sqlite>) -> String {
    use sqlx::{Execute as _, Row as _};
    let mut query = qb.build();
    let args = query.take_arguments().expect("arguments").expect("binds");
    let sql = format!("EXPLAIN QUERY PLAN {}", query.sql().as_str());
    sqlx::query_with(sqlx::AssertSqlSafe(sql), args)
        .fetch_all(db.pool())
        .await
        .expect("explain")
        .iter()
        .map(|row| row.get::<String, _>("detail"))
        .collect::<Vec<_>>()
        .join(" | ")
}

#[cfg(test)]
mod tests;
