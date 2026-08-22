//! [`FerrofinNextUpService`] — the concrete [`NextUpService`] over `ferrofin-db`.
//!
//! Port of `NextUpService`. Computes, per TV series, the "next up" episode for a
//! user given their watch history. The C# service returns deserialized domain
//! `BaseItem`s via `IItemQueryHelpers.DeserializeBaseItem`; the trait here
//! returns raw [`BaseItemEntity`] rows (deserialization is a DTO-layer concern),
//! so no query-helper sibling is taken. The result feeds `TvSeriesManager` in a
//! later unit, which does the presentation-layer assembly.
//!
//! Access-filtering (`ApplyAccessFiltering`) and navigation-loading
//! (`ApplyNavigations`) in C# widen/narrow the row set through the library
//! manager's parental controls; those are not part of this repository seam, so
//! the queries here use the item type + series key + played/virtual predicates
//! that define the next-up algorithm and defer the parental widening to the
//! caller. Episodes are ordered by `(ParentIndexNumber, IndexNumber)` — season,
//! then episode — exactly as C#.
//!
//! # Round-trip budget
//!
//! Every query here is batched over *all* requested series keys — nothing runs
//! per series. One next-up request costs:
//!
//! * 1 query for the eligible series keys ([`get_next_up_series_keys`]);
//! * 1 query for the `(id, season, episode, played, virtual)` projection of every
//!   non-special episode of those series — the played, unplayed and
//!   played-non-virtual pools are partitioned from that single result set;
//! * 1 query for the specials, only when specials were requested;
//! * 1 query for the *played by date* projection, only in rewatching mode;
//! * 1 query for the full rows of the (few) episodes the batch result actually
//!   returns.
//!
//! So 3 round-trips in the default configuration and 5 with rewatching on. The
//! picked-episode positions come from the projection, so no full row has to be
//! loaded just to read its season/episode, and each full row is *moved* into the
//! batch result (cloned only when two fields of the same series name it).
//!
//! [`get_next_up_series_keys`]: NextUpService::get_next_up_series_keys

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_db::store::{datetime_to_db, guid_to_db};
use ferrofin_model::data::BaseItemKind;
use uuid::Uuid;

use ferrofin_traits::error::ServiceError;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::{NextUpEpisodeBatchResult, NextUpService};

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::PLACEHOLDER_ID;

/// The concrete next-up service.
#[derive(Clone)]
pub struct FerrofinNextUpService {
    db: Database,
}

impl std::fmt::Debug for FerrofinNextUpService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinNextUpService")
            .finish_non_exhaustive()
    }
}

impl FerrofinNextUpService {
    /// Creates a next-up service over the given database.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// The stored `Type` name for episodes, or an error if the lookup lacks it.
    fn episode_type() -> Result<&'static str, ServiceError> {
        stored_type_name(BaseItemKind::Episode)
            .ok_or_else(|| ServiceError::backend("no stored type name for Episode"))
    }

    /// Fetches full episode rows for the given ids, keyed by id.
    async fn episodes_by_ids(
        &self,
        ids: &[Uuid],
    ) -> Result<HashMap<Uuid, BaseItemEntity>, ServiceError> {
        let mut map = HashMap::new();
        if ids.is_empty() {
            return Ok(map);
        }
        let mut sql = String::from(r#"SELECT * FROM "BaseItems" WHERE "Id" IN ("#);
        for i in 0..ids.len() {
            if i > 0 {
                sql.push_str(", ");
            }
            sql.push('?');
        }
        sql.push(')');
        let mut query = sqlx::query_as::<_, BaseItemEntity>(&sql);
        for id in ids {
            query = query.bind(guid_to_db(*id));
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        for row in rows {
            if let Ok(id) = Uuid::parse_str(&row.id) {
                map.insert(id, row);
            }
        }
        Ok(map)
    }
}

/// A lightweight `(id, season, episode)` projection used to pick the next/last
/// episode without loading full rows (mirrors the C# anonymous projection).
///
/// `is_real` mirrors the SQL `IsVirtualItem = 0` predicate: only *real* (on-disk)
/// episodes are eligible as a next-up pick, while the last-watched position is
/// taken over played episodes whether or not they are virtual.
#[derive(Clone, Copy)]
struct EpisodePos {
    id: Uuid,
    season: Option<i64>,
    episode: Option<i64>,
    is_real: bool,
}

impl EpisodePos {
    /// The `(season, episode)` position, or `None` when either number is missing
    /// — the same nullability rule the C# uses when comparing positions, so a
    /// last-watched episode with no numbers searches from the start of the series.
    fn position(self) -> Option<(i64, i64)> {
        Some((self.season?, self.episode?))
    }
}

/// A row of the episode-position projection:
/// `(series key, id, season, episode, played, is virtual)`.
type PositionRow = (String, String, Option<i64>, Option<i64>, bool, bool);

/// A row of the played-by-date projection:
/// `(series key, id, season, episode, is virtual, last played)`.
type PlayDateRow = (
    String,
    String,
    Option<i64>,
    Option<i64>,
    bool,
    Option<DateTime<Utc>>,
);

/// The episode pools of one series, partitioned out of the single projection
/// query. `played` keeps virtual rows (the last-watched position may sit on a
/// virtual episode); `unplayed` holds only real, unplayed episodes.
#[derive(Default)]
struct SeriesEpisodes {
    /// Played, non-special episodes — virtual rows included.
    played: Vec<EpisodePos>,
    /// Unplayed, non-special, non-virtual episodes — the next-up candidates.
    unplayed: Vec<EpisodePos>,
}

#[async_trait]
impl NextUpService for FerrofinNextUpService {
    async fn get_next_up_series_keys(
        &self,
        filter: &InternalItemsQuery,
        date_cutoff: DateTime<Utc>,
    ) -> Result<Vec<String>, ServiceError> {
        let Some(user) = filter.user.as_ref() else {
            return Err(ServiceError::invalid_input("next-up requires a user"));
        };
        if filter.top_parent_ids.is_empty() {
            return Ok(Vec::new());
        }
        let episode_type = Self::episode_type()?;

        // Series (by presentation key) whose most-recently-played episode within
        // the requested libraries is at/after the cutoff, newest first.
        let sql = next_up_series_keys_sql(filter.top_parent_ids.len());
        let mut query = sqlx::query_scalar::<_, String>(&sql)
            .bind(episode_type)
            .bind(&user.id)
            .bind(PLACEHOLDER_ID);
        for id in &filter.top_parent_ids {
            query = query.bind(guid_to_db(*id));
        }
        query = query.bind(datetime_to_db(date_cutoff));
        let mut keys = query.fetch_all(self.db.pool()).await.map_err(db_err)?;

        if let Some(limit) = filter.limit {
            keys.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        }
        Ok(keys)
    }

    async fn get_next_up_episodes_batch(
        &self,
        filter: &InternalItemsQuery,
        series_keys: &[String],
        include_specials: bool,
        include_watched_for_rewatching: bool,
    ) -> Result<HashMap<String, NextUpEpisodeBatchResult>, ServiceError> {
        let Some(user) = filter.user.as_ref() else {
            return Err(ServiceError::invalid_input("next-up requires a user"));
        };
        if series_keys.is_empty() {
            return Ok(HashMap::new());
        }
        let episode_type = Self::episode_type()?;
        let user_id = user.id.clone();

        // ── One projection query feeds every position-based decision below ──
        let by_key = self
            .episode_positions(episode_type, &user_id, series_keys)
            .await?;

        // Last watched: highest season/episode among played, non-special rows.
        let mut last_watched_by_key: HashMap<&str, EpisodePos> = HashMap::new();
        for (key, episodes) in &by_key {
            if let Some(last) = highest_position(&episodes.played) {
                last_watched_by_key.insert(key.as_str(), last);
            }
        }

        // ── Last watched by play date (rewatching mode) ──
        let last_watched_by_date = if include_watched_for_rewatching {
            self.last_watched_by_play_date(episode_type, &user_id, series_keys)
                .await?
        } else {
            HashMap::new()
        };

        // ── Specials (season 0), non-virtual ──
        let mut specials_by_key = if include_specials {
            self.fetch_specials(episode_type, series_keys).await?
        } else {
            HashMap::new()
        };

        // ── Next up: first unplayed episode after the last-watched position ──
        let mut next_up_by_key: HashMap<&str, Uuid> = HashMap::new();
        for (key, episodes) in &by_key {
            let after = last_watched_by_key
                .get(key.as_str())
                .and_then(|pos| pos.position());
            if let Some(next) = first_after(&episodes.unplayed, after) {
                next_up_by_key.insert(key.as_str(), next);
            }
        }

        // ── Next played (rewatching): first played episode after last-by-date ──
        let mut next_played_by_key: HashMap<&str, Uuid> = HashMap::new();
        if include_watched_for_rewatching {
            for (key, last) in &last_watched_by_date {
                let Some(episodes) = by_key.get(key) else {
                    continue;
                };
                // The rewatching candidates are the *real* played episodes.
                let candidates: Vec<EpisodePos> = episodes
                    .played
                    .iter()
                    .copied()
                    .filter(|pos| pos.is_real)
                    .collect();
                if let Some(next) = first_after(&candidates, last.position()) {
                    next_played_by_key.insert(key.as_str(), next);
                }
            }
        }

        // ── One fetch of the full rows the batch result actually returns ──
        let mut refs: HashMap<Uuid, usize> = HashMap::new();
        for id in last_watched_by_key
            .values()
            .map(|pos| pos.id)
            .chain(next_up_by_key.values().copied())
            .chain(last_watched_by_date.values().map(|pos| pos.id))
            .chain(next_played_by_key.values().copied())
        {
            *refs.entry(id).or_insert(0) += 1;
        }
        let wanted: Vec<Uuid> = refs.keys().copied().collect();
        let mut rows = self.episodes_by_ids(&wanted).await?;

        // ── Assemble a batch result per series key ──
        let mut result: HashMap<String, NextUpEpisodeBatchResult> = HashMap::new();
        for key in series_keys {
            // Duplicate keys would otherwise re-take rows already moved out.
            if result.contains_key(key) {
                continue;
            }
            let mut batch = NextUpEpisodeBatchResult::default();
            if let Some(pos) = last_watched_by_key.get(key.as_str()) {
                batch.last_watched = take_row(&mut rows, &mut refs, pos.id);
            }
            if let Some(id) = next_up_by_key.get(key.as_str()) {
                batch.next_up = take_row(&mut rows, &mut refs, *id);
            }
            if include_specials {
                batch.specials = specials_by_key.remove(key).unwrap_or_default();
            }
            if include_watched_for_rewatching {
                if let Some(pos) = last_watched_by_date.get(key) {
                    batch.last_watched_for_rewatching = take_row(&mut rows, &mut refs, pos.id);
                }
                if let Some(id) = next_played_by_key.get(key.as_str()) {
                    batch.next_played_for_rewatching = take_row(&mut rows, &mut refs, *id);
                }
            }
            result.insert(key.clone(), batch);
        }
        Ok(result)
    }
}

impl FerrofinNextUpService {
    /// The most-recently-played episode per series key (rewatching mode).
    /// Rows come back date-descending, so the first seen per key is the newest.
    ///
    /// The season/episode numbers ride along so the caller never has to load the
    /// full row just to read the position it must search past.
    async fn last_watched_by_play_date(
        &self,
        episode_type: &str,
        user_id: &str,
        series_keys: &[String],
    ) -> Result<HashMap<String, EpisodePos>, ServiceError> {
        let mut sql = String::from(
            r#"SELECT bi."SeriesPresentationUniqueKey", bi."Id",
                      bi."ParentIndexNumber", bi."IndexNumber", bi."IsVirtualItem",
                      ud."LastPlayedDate"
               FROM "BaseItems" bi
               JOIN "UserData" ud ON ud."ItemId" = bi."Id"
               WHERE bi."Type" = ? AND ud."UserId" = ? AND ud."Played" = 1
                 AND ud."ItemId" <> ? AND bi."ParentIndexNumber" <> 0
                 AND bi."SeriesPresentationUniqueKey" IN ("#,
        );
        push_key_placeholders(&mut sql, series_keys.len());
        sql.push_str(r#") ORDER BY ud."LastPlayedDate" DESC"#);
        let mut query = sqlx::query_as::<_, PlayDateRow>(&sql)
            .bind(episode_type)
            .bind(user_id)
            .bind(PLACEHOLDER_ID);
        for key in series_keys {
            query = query.bind(key.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        let mut out: HashMap<String, EpisodePos> = HashMap::new();
        for (key, id, season, episode, is_virtual, _date) in rows {
            if let Ok(id) = Uuid::parse_str(&id) {
                out.entry(key).or_insert(EpisodePos {
                    id,
                    season,
                    episode,
                    is_real: !is_virtual,
                });
            }
        }
        Ok(out)
    }

    /// The non-virtual special (season 0) episode rows per series key.
    async fn fetch_specials(
        &self,
        episode_type: &str,
        series_keys: &[String],
    ) -> Result<HashMap<String, Vec<BaseItemEntity>>, ServiceError> {
        let mut sql = String::from(
            r#"SELECT * FROM "BaseItems"
               WHERE "Type" = ? AND "ParentIndexNumber" = 0 AND "IsVirtualItem" = 0
                 AND "SeriesPresentationUniqueKey" IN ("#,
        );
        push_key_placeholders(&mut sql, series_keys.len());
        sql.push(')');
        let mut query = sqlx::query_as::<_, BaseItemEntity>(&sql).bind(episode_type);
        for key in series_keys {
            query = query.bind(key.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        let mut out: HashMap<String, Vec<BaseItemEntity>> = HashMap::new();
        for row in rows {
            if let Some(key) = row.series_presentation_unique_key.clone() {
                out.entry(key).or_default().push(row);
            }
        }
        Ok(out)
    }

    /// Fetches the `(id, season, episode, played, virtual)` projection of every
    /// non-special episode of the given series, grouped per series key.
    ///
    /// This is deliberately **one** query covering all three pools the batch
    /// algorithm needs (played incl. virtual, unplayed real, played real): the
    /// played flag is an `EXISTS` over `UserData` — the same predicate the
    /// per-pool queries used to carry in their `WHERE` — evaluated once per row
    /// instead of once per row per pool.
    async fn episode_positions(
        &self,
        episode_type: &str,
        user_id: &str,
        series_keys: &[String],
    ) -> Result<HashMap<String, SeriesEpisodes>, ServiceError> {
        let mut sql = String::from(
            r#"SELECT bi."SeriesPresentationUniqueKey", bi."Id",
                      bi."ParentIndexNumber", bi."IndexNumber",
                      EXISTS (SELECT 1 FROM "UserData" ud WHERE ud."ItemId" = bi."Id"
                              AND ud."UserId" = ? AND ud."Played" = 1) AS "Played",
                      bi."IsVirtualItem"
               FROM "BaseItems" bi
               WHERE bi."Type" = ? AND bi."ParentIndexNumber" <> 0
                 AND bi."SeriesPresentationUniqueKey" IN ("#,
        );
        push_key_placeholders(&mut sql, series_keys.len());
        sql.push(')');

        let mut query = sqlx::query_as::<_, PositionRow>(&sql)
            .bind(user_id)
            .bind(episode_type);
        for key in series_keys {
            query = query.bind(key.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;

        let mut out: HashMap<String, SeriesEpisodes> = HashMap::new();
        for (key, id, season, episode, played, is_virtual) in rows {
            let Ok(id) = Uuid::parse_str(&id) else {
                continue;
            };
            let pos = EpisodePos {
                id,
                season,
                episode,
                is_real: !is_virtual,
            };
            let bucket = out.entry(key).or_default();
            if played {
                bucket.played.push(pos);
            } else if pos.is_real {
                bucket.unplayed.push(pos);
            }
        }
        Ok(out)
    }
}

/// Moves the row for `id` out of `rows`, cloning only while another batch field
/// still references it (the same episode can be both the last watched and the
/// last watched *by date*).
fn take_row(
    rows: &mut HashMap<Uuid, BaseItemEntity>,
    refs: &mut HashMap<Uuid, usize>,
    id: Uuid,
) -> Option<BaseItemEntity> {
    match refs.get_mut(&id) {
        Some(remaining) if *remaining > 1 => {
            *remaining -= 1;
            rows.get(&id).cloned()
        }
        _ => {
            refs.remove(&id);
            rows.remove(&id)
        }
    }
}

/// Appends `?, ?, …` of length `n` (at least one, `NULL` when empty).
fn push_key_placeholders(sql: &mut String, n: usize) {
    if n == 0 {
        sql.push_str("NULL");
        return;
    }
    for i in 0..n {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
    }
}

/// The `(season, episode)` sort key of an episode row, missing numbers sorting
/// lowest (`i64::MIN`), matching the C# nullable ordering.
fn sort_key(season: Option<i64>, episode: Option<i64>) -> (i64, i64) {
    (season.unwrap_or(i64::MIN), episode.unwrap_or(i64::MIN))
}

/// The highest-numbered episode in a group (last watched).
fn highest_position(positions: &[EpisodePos]) -> Option<EpisodePos> {
    positions
        .iter()
        .max_by_key(|p| sort_key(p.season, p.episode))
        .copied()
}

/// The id of the first episode strictly after `after` (or the very first when
/// `after` is `None`), in season/episode order.
fn first_after(positions: &[EpisodePos], after: Option<(i64, i64)>) -> Option<Uuid> {
    let mut sorted: Vec<&EpisodePos> = positions.iter().collect();
    sorted.sort_by_key(|p| sort_key(p.season, p.episode));
    sorted
        .into_iter()
        .find(|p| match after {
            None => true,
            Some(pos) => sort_key(p.season, p.episode) > pos,
        })
        .map(|p| p.id)
}

/// The series-keys aggregate for next-up, with `parents` bound `TopParentId`
/// placeholders.
///
/// Driven from `UserData`, not `BaseItems`, and pinned with CROSS JOIN.
///
/// Left to itself SQLite seeds from `BaseItems` on
/// `(Type, SeriesPresentationUniqueKey)` and then seeks `UserData` once
/// per episode — every episode in the library, however few the user has
/// actually watched. On the bench fixture that is 1,997 seeks for a user
/// with ONE `UserData` row, and it is the single most expensive
/// statement in the request: 0.92 ms of nextup's 2.33 ms CPU, which is
/// what pushed the endpoint past its 4-core budget at the benchmark's
/// 1849 rps and collapsed it to a 1.5-2 s p50.
///
/// Seeding from `UserData (UserId = ?)` instead makes the work scale
/// with what the user has watched rather than with library size — the
/// covering index answers it directly. Same rows either way: both are
/// inner joins over the identical predicates; CROSS JOIN only removes
/// the planner's freedom to reorder them.
#[must_use]
pub fn next_up_series_keys_sql(parents: usize) -> String {
    let mut sql = String::from(
        r#"SELECT bi."SeriesPresentationUniqueKey" AS key,
                  MAX(ud."LastPlayedDate") AS last_played
           FROM "UserData" ud
           CROSS JOIN "BaseItems" bi ON bi."Id" = ud."ItemId"
           WHERE bi."Type" = ? AND ud."UserId" = ?
             AND ud."ItemId" <> ?
             AND bi."SeriesPresentationUniqueKey" IS NOT NULL
             AND bi."TopParentId" IN ("#,
    );
    for i in 0..parents {
        if i > 0 {
            sql.push_str(", ");
        }
        sql.push('?');
    }
    sql.push_str(
        r#") GROUP BY bi."SeriesPresentationUniqueKey"
           HAVING last_played IS NOT NULL AND last_played >= ?
           ORDER BY last_played DESC"#,
    );
    sql
}

#[cfg(test)]
mod tests {
    use super::FerrofinNextUpService;
    use crate::test_support::{
        clear_index_number, corrupt_last_played_date, seed_episode, seed_user, seed_user_data,
        test_db,
    };
    use chrono::{DateTime, Utc};
    use ferrofin_db::entities::users::UserEntity;
    use ferrofin_db::store::guid_to_db;
    use ferrofin_traits::options::InternalItemsQuery;
    use ferrofin_traits::persistence::NextUpService;
    use uuid::Uuid;

    /// Parses an RFC3339 timestamp into a UTC datetime for seeding user-data.
    fn ts(s: &str) -> DateTime<Utc> {
        s.parse().expect("valid timestamp")
    }

    fn query_for(user: UserEntity, top_parent: Uuid) -> InternalItemsQuery {
        InternalItemsQuery {
            user: Some(user),
            top_parent_ids: vec![top_parent],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn series_keys_ordered_by_recent_play() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let s1e1 = Uuid::new_v4();
        let s2e1 = Uuid::new_v4();
        seed_episode(&db, s1e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, s2e1, "series-b", 1, 1, false, Some(top)).await;
        // series-b watched most recently.
        seed_user_data(&db, user_id, s1e1, true, Some(ts("2020-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, s2e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let cutoff = "2000-01-01T00:00:00Z".parse().unwrap();
        let keys = svc
            .get_next_up_series_keys(&query_for(user, top), cutoff)
            .await
            .expect("keys");
        assert_eq!(keys, vec!["series-b".to_owned(), "series-a".to_owned()]);
    }

    #[tokio::test]
    async fn next_up_picks_episode_after_last_watched() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        // Watched e1 and e2 → next up is e3.
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                false,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
    }

    #[tokio::test]
    async fn specials_included_when_requested() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let special = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, special, "series-a", 0, 1, false, Some(top)).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                true,
                false,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(series.specials.len(), 1);
        assert_eq!(series.specials[0].id, guid_to_db(special));
    }

    // ── Semantics pinned below: which episode is "next", and over which pool ──

    /// Runs the batch with specials and rewatching off — the default shape.
    async fn batch_of(
        svc: &FerrofinNextUpService,
        user: UserEntity,
        top: Uuid,
        keys: &[&str],
    ) -> std::collections::HashMap<String, ferrofin_traits::persistence::NextUpEpisodeBatchResult>
    {
        let keys: Vec<String> = keys.iter().map(|k| (*k).to_owned()).collect();
        svc.get_next_up_episodes_batch(&query_for(user, top), &keys, false, false)
            .await
            .expect("batch")
    }

    #[tokio::test]
    async fn next_up_never_picks_a_virtual_episode() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2_virtual = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2_virtual, "series-a", 1, 2, true, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        // The unwatched season-1 episode 2 exists only as a virtual (missing
        // file) row, so the pick skips past it to episode 3.
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
    }

    #[tokio::test]
    async fn last_watched_position_counts_virtual_played_episodes() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2_virtual = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2_virtual, "series-a", 1, 2, true, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        // Only the *virtual* episode 2 is played; episode 1 is still unwatched.
        seed_user_data(
            &db,
            user_id,
            e2_virtual,
            true,
            Some(ts("2021-01-01T00:00:00Z")),
        )
        .await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        // The last-watched pool is *not* filtered to real rows, so the position
        // to search past is (1, 2) — episode 1 is behind it and episode 3 wins.
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2_virtual))
        );
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
    }

    #[tokio::test]
    async fn unnumbered_last_watched_searches_from_the_start() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let watched = Uuid::new_v4();
        let unnumbered = Uuid::new_v4();
        seed_episode(&db, watched, "series-a", 1, 5, false, Some(top)).await;
        seed_episode(&db, unnumbered, "series-a", 1, 9, false, Some(top)).await;
        clear_index_number(&db, watched).await;
        clear_index_number(&db, unnumbered).await;
        seed_user_data(
            &db,
            user_id,
            watched,
            true,
            Some(ts("2021-01-01T00:00:00Z")),
        )
        .await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        // A half-known position yields no comparison point at all (`None`), so
        // the first unplayed candidate is taken even though it does not sort
        // strictly after the last-watched sort key.
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(unnumbered))
        );
    }

    #[tokio::test]
    async fn specials_are_not_next_up_candidates() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let special = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, special, "series-a", 0, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;

        let svc = FerrofinNextUpService::new(db.clone());

        // Nothing is played yet, so there is no last-watched position to search
        // past and the pick is simply the lowest-sorting candidate. Season 0
        // sorts *below* every real season, so the special would win outright
        // unless the projection query excludes it: this is the shape where the
        // `ParentIndexNumber <> 0` guard — not the ordering — is load-bearing.
        let batch = batch_of(&svc, user.clone(), top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        assert!(series.last_watched.is_none(), "nothing has been played");
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e1)),
            "an unwatched series starts at its first real episode, not its special"
        );

        // And mid-series the special is still not a candidate.
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
    }

    #[tokio::test]
    async fn fully_watched_series_has_no_next_up() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a"]).await;
        let series = batch.get("series-a").expect("series present");
        assert!(series.next_up.is_none());
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
    }

    #[tokio::test]
    async fn each_series_in_a_batch_is_resolved_independently() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let a1 = Uuid::new_v4();
        let a2 = Uuid::new_v4();
        let b1 = Uuid::new_v4();
        let b2 = Uuid::new_v4();
        seed_episode(&db, a1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, a2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, b1, "series-b", 2, 7, false, Some(top)).await;
        seed_episode(&db, b2, "series-b", 2, 8, false, Some(top)).await;
        seed_user_data(&db, user_id, a1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, b1, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = batch_of(&svc, user, top, &["series-a", "series-b"]).await;
        assert_eq!(
            batch["series-a"].next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(a2))
        );
        assert_eq!(
            batch["series-b"].next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(b2))
        );
        // Neither series' pool leaks into the other's last-watched position.
        assert_eq!(
            batch["series-b"]
                .last_watched
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(b1))
        );
    }

    #[tokio::test]
    async fn repeated_series_key_still_yields_the_full_result() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let special = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, special, "series-a", 0, 1, false, Some(top)).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let keys = vec!["series-a".to_owned(), "series-a".to_owned()];
        let batch = svc
            .get_next_up_episodes_batch(&query_for(user, top), &keys, true, false)
            .await
            .expect("batch");
        assert_eq!(batch.len(), 1);
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e1))
        );
        assert_eq!(series.specials.len(), 1);
    }

    #[tokio::test]
    async fn rewatching_resumes_after_the_most_recently_played_episode() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        // Whole series watched, then episode 1 re-watched most recently.
        seed_user_data(&db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e3, true, Some(ts("2021-01-03T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-06-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                true,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert!(series.next_up.is_none(), "nothing is unwatched");
        // Highest position is still episode 3 …
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
        // … but the rewatch pick follows the most recent *play date* (episode 1).
        assert_eq!(
            series
                .last_watched_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e1))
        );
        assert_eq!(
            series
                .next_played_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
    }

    #[tokio::test]
    async fn rewatching_never_picks_a_virtual_episode() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2_virtual = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2_virtual, "series-a", 1, 2, true, Some(top)).await;
        seed_episode(&db, e3, "series-a", 1, 3, false, Some(top)).await;
        seed_user_data(
            &db,
            user_id,
            e2_virtual,
            true,
            Some(ts("2021-01-02T00:00:00Z")),
        )
        .await;
        seed_user_data(&db, user_id, e3, true, Some(ts("2021-01-03T00:00:00Z"))).await;
        // Episode 1 re-watched last, so the rewatch search starts after (1, 1).
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-06-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                true,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        // Episode 2 is played but virtual, so the rewatch pick skips to 3 — the
        // rewatch candidate pool is filtered to real rows even though the
        // last-watched pool is not.
        assert_eq!(
            series
                .next_played_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e3))
        );
    }

    // `corrupt_last_played_date` (in `test_support`) is what the play-date gate
    // below rests on: `LastPlayedDate` is selected by exactly one query in the
    // batch path — the play-date projection the rewatching flag gates — so a
    // batch that still succeeds against the corrupted column is proof that
    // round-trip was never issued.

    /// Seeds `series-a` as watched through episode 2 with episode 1 re-watched
    /// most recently, so both rewatching fields have a real answer to report
    /// (`last_watched_for_rewatching` = e1, `next_played_for_rewatching` = e2),
    /// and returns `(user, top parent, e1, e2)`. Episode 3 stays unplayed so the
    /// non-rewatching `next_up` still has an answer too.
    async fn seed_rewatched_series(db: &ferrofin_db::Database) -> (UserEntity, Uuid, Uuid, Uuid) {
        let top = Uuid::new_v4();
        let user = seed_user(db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        let e3 = Uuid::new_v4();
        seed_episode(db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(db, e2, "series-a", 1, 2, false, Some(top)).await;
        seed_episode(db, e3, "series-a", 1, 3, false, Some(top)).await;
        seed_user_data(db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;
        seed_user_data(db, user_id, e1, true, Some(ts("2021-06-01T00:00:00Z"))).await;
        (user, top, e1, e2)
    }

    /// The *compute* gate: the play-date round-trip must not be issued at all
    /// when rewatching was not asked for. Covered independently of the assembly
    /// gate — the assertions here are about which query runs, not about which
    /// fields the batch result carries.
    #[tokio::test]
    async fn rewatching_play_date_query_is_only_issued_when_requested() {
        let db = test_db().await;
        let (user, top, e1, e2) = seed_rewatched_series(&db).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();
        corrupt_last_played_date(&db, user_id, e1).await;

        let svc = FerrofinNextUpService::new(db);
        let keys = ["series-a".to_owned()];

        // Rewatching off: the unreadable column is never selected, so the batch
        // resolves normally off the position projection alone.
        let batch = svc
            .get_next_up_episodes_batch(&query_for(user.clone(), top), &keys, false, false)
            .await
            .expect("batch resolves without touching LastPlayedDate");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );

        // Rewatching on: the same call now reads the column and fails — which is
        // what makes the success above evidence of a skipped query rather than
        // of a harmless one.
        let err = svc
            .get_next_up_episodes_batch(&query_for(user, top), &keys, false, true)
            .await;
        assert!(
            err.is_err(),
            "the play-date query must run when rewatching is requested"
        );
    }

    /// The *assembly* gate: the rewatching fields of the batch result are filled
    /// in exactly when rewatching was requested. Same database, same seeded
    /// history, only the flag differs — so the pair discriminates the gate on
    /// its own, without depending on the compute gate upstream.
    #[tokio::test]
    async fn rewatching_fields_follow_the_request_flag() {
        let db = test_db().await;
        let (user, top, e1, e2) = seed_rewatched_series(&db).await;

        let svc = FerrofinNextUpService::new(db);
        let keys = ["series-a".to_owned()];

        let batch = svc
            .get_next_up_episodes_batch(&query_for(user.clone(), top), &keys, false, false)
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert!(series.last_watched_for_rewatching.is_none());
        assert!(series.next_played_for_rewatching.is_none());
        assert!(series.specials.is_empty(), "specials were not requested");

        let batch = svc
            .get_next_up_episodes_batch(&query_for(user, top), &keys, false, true)
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series
                .last_watched_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e1))
        );
        assert_eq!(
            series
                .next_played_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
    }

    #[tokio::test]
    async fn last_watched_and_rewatch_pick_can_be_the_same_row() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let e1 = Uuid::new_v4();
        let e2 = Uuid::new_v4();
        seed_episode(&db, e1, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, e2, "series-a", 1, 2, false, Some(top)).await;
        // Episode 2 is both the highest played and the most recently played, so
        // `last_watched` and `last_watched_for_rewatching` name the same row.
        seed_user_data(&db, user_id, e1, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, e2, true, Some(ts("2021-01-02T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let batch = svc
            .get_next_up_episodes_batch(
                &query_for(user, top),
                &["series-a".to_owned()],
                false,
                true,
            )
            .await
            .expect("batch");
        let series = batch.get("series-a").expect("series present");
        assert_eq!(
            series.last_watched.as_ref().map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        assert_eq!(
            series
                .last_watched_for_rewatching
                .as_ref()
                .map(|e| e.id.clone()),
            Some(guid_to_db(e2))
        );
        assert!(series.next_played_for_rewatching.is_none());
    }

    #[tokio::test]
    async fn series_keys_respect_the_date_cutoff_and_limit() {
        let db = test_db().await;
        let top = Uuid::new_v4();
        let user = seed_user(&db, Uuid::new_v4()).await;
        let user_id = Uuid::parse_str(&user.id).unwrap();

        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let c = Uuid::new_v4();
        seed_episode(&db, a, "series-a", 1, 1, false, Some(top)).await;
        seed_episode(&db, b, "series-b", 1, 1, false, Some(top)).await;
        seed_episode(&db, c, "series-c", 1, 1, false, Some(top)).await;
        seed_user_data(&db, user_id, a, true, Some(ts("2019-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, b, true, Some(ts("2021-01-01T00:00:00Z"))).await;
        seed_user_data(&db, user_id, c, true, Some(ts("2022-01-01T00:00:00Z"))).await;

        let svc = FerrofinNextUpService::new(db);
        let mut query = query_for(user, top);
        query.limit = Some(1);
        let keys = svc
            .get_next_up_series_keys(&query, ts("2020-01-01T00:00:00Z"))
            .await
            .expect("keys");
        // series-a is behind the cutoff; the limit keeps only the newest of the
        // remaining two, newest-first.
        assert_eq!(keys, vec!["series-c".to_owned()]);
    }
}
