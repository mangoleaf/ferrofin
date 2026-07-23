//! [`HermitNextUpService`] — the concrete [`NextUpService`] over `hermit-db`.
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

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use hermit_db::Database;
use hermit_db::entities::base_items::BaseItemEntity;
use hermit_model::data::BaseItemKind;
use uuid::Uuid;

use hermit_traits::error::ServiceError;
use hermit_traits::options::InternalItemsQuery;
use hermit_traits::persistence::{NextUpEpisodeBatchResult, NextUpService};

use crate::db_error::db_err;
use crate::item_type_lookup::stored_type_name;
use crate::translate_query::PLACEHOLDER_ID;

/// The concrete next-up service.
#[derive(Clone)]
pub struct HermitNextUpService {
    db: Database,
}

impl std::fmt::Debug for HermitNextUpService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HermitNextUpService")
            .finish_non_exhaustive()
    }
}

impl HermitNextUpService {
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
            query = query.bind(id.to_string());
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
struct EpisodePos {
    id: Uuid,
    season: Option<i64>,
    episode: Option<i64>,
}

#[async_trait]
impl NextUpService for HermitNextUpService {
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
        let mut sql = String::from(
            r#"SELECT bi."SeriesPresentationUniqueKey" AS key,
                      MAX(ud."LastPlayedDate") AS last_played
               FROM "BaseItems" bi
               JOIN "UserData" ud ON ud."ItemId" = bi."Id"
               WHERE bi."Type" = ? AND ud."UserId" = ?
                 AND ud."ItemId" <> ?
                 AND bi."SeriesPresentationUniqueKey" IS NOT NULL
                 AND bi."TopParentId" IN ("#,
        );
        for i in 0..filter.top_parent_ids.len() {
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

        let mut query = sqlx::query_scalar::<_, String>(&sql)
            .bind(episode_type)
            .bind(&user.id)
            .bind(PLACEHOLDER_ID);
        for id in &filter.top_parent_ids {
            query = query.bind(id.to_string());
        }
        query = query.bind(date_cutoff);
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

        // ── Last watched (highest season/episode among played, non-special) ──
        let played = self
            .episode_positions(episode_type, &user_id, series_keys, Played::Only, false)
            .await?;
        let mut last_watched_by_key: HashMap<String, Uuid> = HashMap::new();
        for (key, positions) in group_by_key(played) {
            if let Some(last) = highest_position(&positions) {
                last_watched_by_key.insert(key, last);
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

        // Full rows for the last-watched episodes, to read their season/episode.
        let last_watched_ids: Vec<Uuid> = last_watched_by_key
            .values()
            .chain(last_watched_by_date.values())
            .copied()
            .collect();
        let last_watched_rows = self.episodes_by_ids(&last_watched_ids).await?;

        // ── Next up: first unplayed episode after the last-watched position ──
        let unplayed = self
            .episode_positions(episode_type, &user_id, series_keys, Played::Not, true)
            .await?;
        let unplayed_by_key = group_by_key(unplayed);
        let mut next_up_by_key: HashMap<String, Uuid> = HashMap::new();
        for key in series_keys {
            let Some(candidates) = unplayed_by_key.get(key) else {
                continue;
            };
            let after = last_watched_by_key
                .get(key)
                .and_then(|id| last_watched_rows.get(id))
                .and_then(position_of);
            if let Some(next) = first_after(candidates, after) {
                next_up_by_key.insert(key.clone(), next);
            }
        }

        // ── Next played (rewatching): first played episode after last-by-date ──
        let next_played_by_key = if include_watched_for_rewatching {
            self.next_played_for_rewatching(
                episode_type,
                &user_id,
                series_keys,
                &last_watched_by_date,
                &last_watched_rows,
            )
            .await?
        } else {
            HashMap::new()
        };

        // Full rows for the chosen next episodes.
        let next_ids: Vec<Uuid> = next_up_by_key
            .values()
            .chain(next_played_by_key.values())
            .copied()
            .collect();
        let next_rows = self.episodes_by_ids(&next_ids).await?;

        // ── Assemble a batch result per series key ──
        let mut result = HashMap::new();
        for key in series_keys {
            let mut batch = NextUpEpisodeBatchResult::default();
            if let Some(id) = last_watched_by_key.get(key) {
                batch.last_watched = last_watched_rows.get(id).cloned();
            }
            if let Some(id) = next_up_by_key.get(key) {
                batch.next_up = next_rows.get(id).cloned();
            }
            if include_specials {
                batch.specials = specials_by_key.remove(key).unwrap_or_default();
            }
            if include_watched_for_rewatching {
                if let Some(id) = last_watched_by_date.get(key) {
                    batch.last_watched_for_rewatching = last_watched_rows.get(id).cloned();
                }
                if let Some(id) = next_played_by_key.get(key) {
                    batch.next_played_for_rewatching = next_rows.get(id).cloned();
                }
            }
            result.insert(key.clone(), batch);
        }
        Ok(result)
    }
}

/// Whether the episode-position query restricts to played or unplayed rows.
#[derive(Clone, Copy)]
enum Played {
    /// Only episodes the user has played.
    Only,
    /// Only episodes the user has not played.
    Not,
}

impl HermitNextUpService {
    /// The most-recently-played episode id per series key (rewatching mode).
    /// Rows come back date-descending, so the first seen per key is the newest.
    async fn last_watched_by_play_date(
        &self,
        episode_type: &str,
        user_id: &str,
        series_keys: &[String],
    ) -> Result<HashMap<String, Uuid>, ServiceError> {
        let mut sql = String::from(
            r#"SELECT bi."SeriesPresentationUniqueKey", bi."Id", ud."LastPlayedDate"
               FROM "BaseItems" bi
               JOIN "UserData" ud ON ud."ItemId" = bi."Id"
               WHERE bi."Type" = ? AND ud."UserId" = ? AND ud."Played" = 1
                 AND ud."ItemId" <> ? AND bi."ParentIndexNumber" <> 0
                 AND bi."SeriesPresentationUniqueKey" IN ("#,
        );
        push_key_placeholders(&mut sql, series_keys.len());
        sql.push_str(r#") ORDER BY ud."LastPlayedDate" DESC"#);
        let mut query = sqlx::query_as::<_, (String, String, Option<DateTime<Utc>>)>(&sql)
            .bind(episode_type)
            .bind(user_id)
            .bind(PLACEHOLDER_ID);
        for key in series_keys {
            query = query.bind(key.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;
        let mut out: HashMap<String, Uuid> = HashMap::new();
        for (key, id, _date) in rows {
            if let Ok(id) = Uuid::parse_str(&id) {
                out.entry(key).or_insert(id);
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

    /// The next *played* episode after the last-watched-by-date position, per
    /// series key — the rewatching-mode analogue of the next-up computation.
    async fn next_played_for_rewatching(
        &self,
        episode_type: &str,
        user_id: &str,
        series_keys: &[String],
        last_watched_by_date: &HashMap<String, Uuid>,
        last_watched_rows: &HashMap<Uuid, BaseItemEntity>,
    ) -> Result<HashMap<String, Uuid>, ServiceError> {
        let played_all = self
            .episode_positions(episode_type, user_id, series_keys, Played::Only, true)
            .await?;
        let played_by_key = group_by_key(played_all);
        let mut out: HashMap<String, Uuid> = HashMap::new();
        for key in series_keys {
            let Some(last_by_date_id) = last_watched_by_date.get(key) else {
                continue;
            };
            let Some(last_row) = last_watched_rows.get(last_by_date_id) else {
                continue;
            };
            let Some(candidates) = played_by_key.get(key) else {
                continue;
            };
            if let Some(next) = first_after(candidates, position_of(last_row)) {
                out.insert(key.clone(), next);
            }
        }
        Ok(out)
    }

    /// Fetches `(id, season, episode)` projections of episodes for the given
    /// series keys, restricted to played/unplayed and (optionally) non-virtual,
    /// non-special rows.
    async fn episode_positions(
        &self,
        episode_type: &str,
        user_id: &str,
        series_keys: &[String],
        played: Played,
        exclude_specials: bool,
    ) -> Result<Vec<(String, EpisodePos)>, ServiceError> {
        let played_pred = match played {
            Played::Only => {
                r#"EXISTS (SELECT 1 FROM "UserData" ud WHERE ud."ItemId" = bi."Id"
                       AND ud."UserId" = ? AND ud."Played" = 1)"#
            }
            Played::Not => {
                r#"NOT EXISTS (SELECT 1 FROM "UserData" ud WHERE ud."ItemId" = bi."Id"
                       AND ud."UserId" = ? AND ud."Played" = 1)"#
            }
        };
        let mut sql = format!(
            r#"SELECT bi."SeriesPresentationUniqueKey", bi."Id",
                      bi."ParentIndexNumber", bi."IndexNumber"
               FROM "BaseItems" bi
               WHERE bi."Type" = ? AND bi."ParentIndexNumber" <> 0 AND {played_pred}"#
        );
        if exclude_specials {
            sql.push_str(r#" AND bi."IsVirtualItem" = 0"#);
        }
        sql.push_str(r#" AND bi."SeriesPresentationUniqueKey" IN ("#);
        push_key_placeholders(&mut sql, series_keys.len());
        sql.push(')');

        let mut query = sqlx::query_as::<_, (String, String, Option<i64>, Option<i64>)>(&sql)
            .bind(episode_type)
            .bind(user_id);
        for key in series_keys {
            query = query.bind(key.clone());
        }
        let rows = query.fetch_all(self.db.pool()).await.map_err(db_err)?;

        let mut out = Vec::with_capacity(rows.len());
        for (key, id, season, episode) in rows {
            if let Ok(id) = Uuid::parse_str(&id) {
                out.push((
                    key,
                    EpisodePos {
                        id,
                        season,
                        episode,
                    },
                ));
            }
        }
        Ok(out)
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

/// Groups `(key, pos)` pairs into a map keyed by series presentation key.
fn group_by_key(rows: Vec<(String, EpisodePos)>) -> HashMap<String, Vec<EpisodePos>> {
    let mut map: HashMap<String, Vec<EpisodePos>> = HashMap::new();
    for (key, pos) in rows {
        map.entry(key).or_default().push(pos);
    }
    map
}

/// The `(season, episode)` sort key of an episode row, missing numbers sorting
/// lowest (`i64::MIN`), matching the C# nullable ordering.
fn sort_key(season: Option<i64>, episode: Option<i64>) -> (i64, i64) {
    (season.unwrap_or(i64::MIN), episode.unwrap_or(i64::MIN))
}

/// The `(season, episode)` of a full episode row.
fn position_of(row: &BaseItemEntity) -> Option<(i64, i64)> {
    Some((row.parent_index_number?, row.index_number?))
}

/// The id of the highest-numbered episode in a group (last watched).
fn highest_position(positions: &[EpisodePos]) -> Option<Uuid> {
    positions
        .iter()
        .max_by_key(|p| sort_key(p.season, p.episode))
        .map(|p| p.id)
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

#[cfg(test)]
mod tests {
    use super::HermitNextUpService;
    use crate::test_support::{seed_episode, seed_user, seed_user_data, test_db};
    use chrono::{DateTime, Utc};
    use hermit_db::entities::users::UserEntity;
    use hermit_traits::options::InternalItemsQuery;
    use hermit_traits::persistence::NextUpService;
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

        let svc = HermitNextUpService::new(db);
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

        let svc = HermitNextUpService::new(db);
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
            Some(e2.to_string())
        );
        assert_eq!(
            series.next_up.as_ref().map(|e| e.id.clone()),
            Some(e3.to_string())
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

        let svc = HermitNextUpService::new(db);
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
        assert_eq!(series.specials[0].id, special.to_string());
    }
}
