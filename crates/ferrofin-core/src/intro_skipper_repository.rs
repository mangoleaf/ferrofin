//! The SQLite [`IntroSkipperStore`]: the Intro Skipper's own tier in
//! Ferrofin-owned tables — per-season state
//! (`FerrofinIntroSkipperSeasonStates`, migration 0038), segments
//! (`FerrofinIntroSkipperSegments`) and disabled episodes
//! (`FerrofinIntroSkipperDisabledEpisodes`, migration 0037). Port of the
//! plugin's `Plugin.cs` database methods; the update rules are
//! [`decide_update`], shared with the in-memory store.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_model::intro_skipper::{AnalysisMode, AnalyzerAction};
use ferrofin_model::json::enums::JsonEnum as _;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::intro_skipper::{
    IntroSkipperStore, SEGMENT_COMPARISON_EPSILON, SeasonState, StoredSegment, UpdateDecision,
    decide_update,
};
use uuid::Uuid;

use crate::db_error::db_err;

/// The database-backed [`IntroSkipperStore`].
#[derive(Clone)]
pub struct FerrofinIntroSkipperStore {
    db: Database,
}

impl std::fmt::Debug for FerrofinIntroSkipperStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FerrofinIntroSkipperStore")
            .finish_non_exhaustive()
    }
}

impl FerrofinIntroSkipperStore {
    /// A store over `db`.
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

/// A `FerrofinIntroSkipperSegments` row.
type SegmentRow = (String, i32, f64, f64, bool, String);

/// The columns [`SegmentRow`] reads.
const SEGMENT_COLUMNS: &str = r#""ItemId", "Type", "Start", "End", "IsUserProvided", "ConfigHash""#;

/// A row whose `ItemId` is not a GUID is skipped (with a warning) rather than
/// read as the nil id.
fn to_segment(
    (item_id, mode, start, end, is_user_provided, config_hash): SegmentRow,
) -> Option<StoredSegment> {
    let Ok(id) = Uuid::parse_str(&item_id) else {
        tracing::warn!(
            item_id,
            "intro skipper: a stored segment's item id is not a GUID"
        );
        return None;
    };
    Some(StoredSegment {
        item_id: id,
        mode: AnalysisMode::from_discriminant(mode),
        start,
        end,
        is_user_provided,
        config_hash,
    })
}

/// A stored JSON id list (an unreadable one reads as empty, so the episodes
/// are simply analysed again).
fn id_set(json: &str) -> HashSet<Uuid> {
    serde_json::from_str::<Vec<String>>(json)
        .unwrap_or_default()
        .iter()
        .filter_map(|id| Uuid::parse_str(id).ok())
        .collect()
}

/// `ids` as a JSON array for `json_each`.
fn json_ids(ids: &[Uuid]) -> Result<String, ServiceError> {
    let ids: Vec<String> = ids.iter().map(Uuid::to_string).collect();
    serde_json::to_string(&ids).map_err(|e| ServiceError::backend(e.to_string()))
}

#[async_trait]
impl IntroSkipperStore for FerrofinIntroSkipperStore {
    async fn analyzer_actions(
        &self,
        season_id: Uuid,
    ) -> Result<HashMap<AnalysisMode, AnalyzerAction>, ServiceError> {
        let rows: Vec<(i32, i32)> = sqlx::query_as(
            r#"SELECT "Type", "Action" FROM "FerrofinIntroSkipperSeasonStates"
               WHERE "SeasonId" = ?1"#,
        )
        .bind(season_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        // Any stored integer round-trips: C# keeps undefined enum values too.
        Ok(rows
            .into_iter()
            .map(|(mode, action)| {
                (
                    AnalysisMode::from_discriminant(mode),
                    AnalyzerAction::from_discriminant(action),
                )
            })
            .collect())
    }

    async fn set_analyzer_actions(
        &self,
        season_id: Uuid,
        actions: &[(AnalysisMode, AnalyzerAction)],
    ) -> Result<(), ServiceError> {
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        for (mode, action) in actions {
            sqlx::query(
                r#"INSERT INTO "FerrofinIntroSkipperSeasonStates" ("SeasonId", "Type", "Action")
                   VALUES (?1, ?2, ?3)
                   ON CONFLICT ("SeasonId", "Type") DO UPDATE SET "Action" = excluded."Action""#,
            )
            .bind(season_id.to_string())
            .bind(mode.value())
            .bind(action.value())
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn retain_seasons(&self, season_ids: &[Uuid]) -> Result<(), ServiceError> {
        let ids = json_ids(season_ids)?;
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        for table in [
            "FerrofinIntroSkipperSeasonStates",
            "FerrofinIntroSkipperDisabledEpisodes",
        ] {
            sqlx::query(sqlx::AssertSqlSafe(format!(
                r#"DELETE FROM "{table}" WHERE "SeasonId" NOT IN (SELECT value FROM json_each(?1))"#
            )))
            .bind(&ids)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn update_timestamp(&self, segment: StoredSegment) -> Result<bool, ServiceError> {
        // One transaction: the rules read the item's rows, then write.
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        let existing: Vec<SegmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            r#"SELECT {SEGMENT_COLUMNS} FROM "FerrofinIntroSkipperSegments" WHERE "ItemId" = ?1"#
        )))
        .bind(segment.item_id.to_string())
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let existing: Vec<StoredSegment> = existing.into_iter().filter_map(to_segment).collect();
        match decide_update(
            &existing,
            segment.mode,
            segment.start,
            segment.end,
            segment.is_user_provided,
        ) {
            UpdateDecision::Skip => return Ok(false),
            UpdateDecision::Replace => {
                sqlx::query(
                    r#"DELETE FROM "FerrofinIntroSkipperSegments" WHERE "ItemId" = ?1 AND "Type" = ?2"#,
                )
                .bind(segment.item_id.to_string())
                .bind(segment.mode.value())
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            }
            UpdateDecision::Insert => {}
        }
        sqlx::query(
            r#"INSERT INTO "FerrofinIntroSkipperSegments"
               ("ItemId", "Type", "Start", "End", "IsUserProvided", "ConfigHash")
               VALUES (?1, ?2, ?3, ?4, ?5, ?6)"#,
        )
        .bind(segment.item_id.to_string())
        .bind(segment.mode.value())
        .bind(segment.start)
        .bind(segment.end)
        .bind(segment.is_user_provided)
        .bind(&segment.config_hash)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)?;
        Ok(true)
    }

    async fn segments(&self, item_id: Uuid) -> Result<Vec<StoredSegment>, ServiceError> {
        let rows: Vec<SegmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            r#"SELECT {SEGMENT_COLUMNS} FROM "FerrofinIntroSkipperSegments" WHERE "ItemId" = ?1"#
        )))
        .bind(item_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().filter_map(to_segment).collect())
    }

    async fn segments_of(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<StoredSegment>>, ServiceError> {
        let rows: Vec<SegmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            r#"SELECT {SEGMENT_COLUMNS} FROM "FerrofinIntroSkipperSegments"
               WHERE "ItemId" IN (SELECT value FROM json_each(?1))"#
        )))
        .bind(json_ids(item_ids)?)
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        let mut by_item: HashMap<Uuid, Vec<StoredSegment>> = HashMap::new();
        for segment in rows.into_iter().filter_map(to_segment) {
            by_item.entry(segment.item_id).or_default().push(segment);
        }
        Ok(by_item)
    }

    async fn segments_unless_excluded(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<StoredSegment>, ServiceError> {
        let rows: Vec<SegmentRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            r#"SELECT {SEGMENT_COLUMNS} FROM "FerrofinIntroSkipperSegments"
               WHERE "ItemId" = ?1 AND NOT EXISTS (
                   SELECT 1 FROM "FerrofinIntroSkipperDisabledEpisodes" WHERE "EpisodeId" = ?1)"#
        )))
        .bind(item_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows.into_iter().filter_map(to_segment).collect())
    }

    async fn delete_timestamp(
        &self,
        item_id: Uuid,
        mode: AnalysisMode,
        range: Option<(f64, f64)>,
    ) -> Result<u64, ServiceError> {
        if range.is_none() && mode == AnalysisMode::Commercial {
            return Ok(0);
        }
        let (start, end) = range.unwrap_or_default();
        let result = sqlx::query(
            r#"DELETE FROM "FerrofinIntroSkipperSegments"
               WHERE "ItemId" = ?1 AND "Type" = ?2
                 AND (?3 = 0 OR (abs("Start" - ?4) <= ?6 AND abs("End" - ?5) <= ?6))"#,
        )
        .bind(item_id.to_string())
        .bind(mode.value())
        .bind(range.is_some())
        .bind(start)
        .bind(end)
        .bind(SEGMENT_COMPARISON_EPSILON)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(result.rows_affected())
    }

    async fn delete_mode(&self, mode: AnalysisMode) -> Result<u64, ServiceError> {
        let result = sqlx::query(r#"DELETE FROM "FerrofinIntroSkipperSegments" WHERE "Type" = ?1"#)
            .bind(mode.value())
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(result.rows_affected())
    }

    async fn delete_items(&self, item_ids: &[Uuid]) -> Result<u64, ServiceError> {
        let result = sqlx::query(
            r#"DELETE FROM "FerrofinIntroSkipperSegments"
               WHERE "ItemId" IN (SELECT value FROM json_each(?1))"#,
        )
        .bind(json_ids(item_ids)?)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(result.rows_affected())
    }

    async fn excluded_episodes(&self, season_id: Uuid) -> Result<HashSet<Uuid>, ServiceError> {
        let rows: Vec<(String,)> = sqlx::query_as(
            r#"SELECT "EpisodeId" FROM "FerrofinIntroSkipperDisabledEpisodes" WHERE "SeasonId" = ?1"#,
        )
        .bind(season_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|(id,)| Uuid::parse_str(&id).ok())
            .collect())
    }

    async fn set_excluded(
        &self,
        season_id: Uuid,
        episode_id: Uuid,
        excluded: bool,
    ) -> Result<(), ServiceError> {
        let sql = if excluded {
            r#"INSERT OR IGNORE INTO "FerrofinIntroSkipperDisabledEpisodes" ("SeasonId", "EpisodeId")
               VALUES (?1, ?2)"#
        } else {
            r#"DELETE FROM "FerrofinIntroSkipperDisabledEpisodes"
               WHERE "SeasonId" = ?1 AND "EpisodeId" = ?2"#
        };
        sqlx::query(sql)
            .bind(season_id.to_string())
            .bind(episode_id.to_string())
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    async fn stored_item_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        let rows: Vec<(String,)> =
            sqlx::query_as(r#"SELECT DISTINCT "ItemId" FROM "FerrofinIntroSkipperSegments""#)
                .fetch_all(self.db.pool())
                .await
                .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .filter_map(|(id,)| Uuid::parse_str(&id).ok())
            .collect())
    }

    async fn prune_invalid(&self) -> Result<u64, ServiceError> {
        let result = sqlx::query(r#"DELETE FROM "FerrofinIntroSkipperSegments" WHERE "End" <= 0"#)
            .execute(self.db.writer())
            .await
            .map_err(db_err)?;
        Ok(result.rows_affected())
    }
    async fn season_states(
        &self,
        season_id: Uuid,
    ) -> Result<HashMap<AnalysisMode, SeasonState>, ServiceError> {
        let rows: Vec<(i32, i32, String, String, String)> = sqlx::query_as(
            r#"SELECT "Type", "Action", "EpisodeIds", "ConfigHash", "SettledReanalysisEpisodeIds"
               FROM "FerrofinIntroSkipperSeasonStates" WHERE "SeasonId" = ?1"#,
        )
        .bind(season_id.to_string())
        .fetch_all(self.db.pool())
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(mode, action, episodes, config_hash, settled)| {
                (
                    AnalysisMode::from_discriminant(mode),
                    SeasonState {
                        action: AnalyzerAction::from_discriminant(action),
                        episode_ids: id_set(&episodes),
                        config_hash,
                        settled_episode_ids: id_set(&settled),
                    },
                )
            })
            .collect())
    }

    async fn set_episode_ids(
        &self,
        season_id: Uuid,
        mode: AnalysisMode,
        episode_ids: &[Uuid],
        config_hash: &str,
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"INSERT INTO "FerrofinIntroSkipperSeasonStates" ("SeasonId", "Type", "EpisodeIds", "ConfigHash")
               VALUES (?1, ?2, ?3, ?4)
               ON CONFLICT ("SeasonId", "Type") DO UPDATE SET
                   "EpisodeIds" = excluded."EpisodeIds", "ConfigHash" = excluded."ConfigHash""#,
        )
        .bind(season_id.to_string())
        .bind(mode.value())
        .bind(json_ids(episode_ids)?)
        .bind(config_hash)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn remove_episode_ids(
        &self,
        season_id: Option<Uuid>,
        mode: Option<AnalysisMode>,
        episode_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        sqlx::query(
            r#"UPDATE "FerrofinIntroSkipperSeasonStates"
               SET "EpisodeIds" = (
                   SELECT coalesce(json_group_array(e.value), '[]') FROM json_each("EpisodeIds") e
                   WHERE e.value NOT IN (SELECT value FROM json_each(?3)))
               WHERE (?1 IS NULL OR "SeasonId" = ?1) AND (?2 IS NULL OR "Type" = ?2)"#,
        )
        .bind(season_id.map(|s| s.to_string()))
        .bind(mode.map(AnalysisMode::value))
        .bind(json_ids(episode_ids)?)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn clear_episode_ids(
        &self,
        season_id: Uuid,
        modes: Option<&[AnalysisMode]>,
    ) -> Result<(), ServiceError> {
        let modes: Option<Vec<i32>> = modes.map(|m| m.iter().map(|mode| mode.value()).collect());
        sqlx::query(
            r#"UPDATE "FerrofinIntroSkipperSeasonStates" SET "EpisodeIds" = '[]'
               WHERE "SeasonId" = ?1 AND (?2 IS NULL OR "Type" IN (SELECT value FROM json_each(?2)))"#,
        )
        .bind(season_id.to_string())
        .bind(
            modes
                .map(|m| serde_json::to_string(&m))
                .transpose()
                .map_err(|e| ServiceError::backend(e.to_string()))?,
        )
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn clean_stale_automatic(
        &self,
        item_ids: &[Uuid],
        mode: AnalysisMode,
        config_hash: &str,
    ) -> Result<u64, ServiceError> {
        let result = sqlx::query(
            r#"DELETE FROM "FerrofinIntroSkipperSegments"
               WHERE "ItemId" IN (SELECT value FROM json_each(?1)) AND "Type" = ?2
                 AND "IsUserProvided" = 0 AND "ConfigHash" <> ?3"#,
        )
        .bind(json_ids(item_ids)?)
        .bind(mode.value())
        .bind(config_hash)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(result.rows_affected())
    }

    async fn record_settled(
        &self,
        season_id: Uuid,
        modes: &[AnalysisMode],
        episode_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        let ids = json_ids(episode_ids)?;
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        for mode in modes {
            sqlx::query(
                r#"INSERT INTO "FerrofinIntroSkipperSeasonStates"
                   ("SeasonId", "Type", "SettledReanalysisEpisodeIds") VALUES (?1, ?2, ?3)
                   ON CONFLICT ("SeasonId", "Type") DO UPDATE SET
                       "SettledReanalysisEpisodeIds" = excluded."SettledReanalysisEpisodeIds""#,
            )
            .bind(season_id.to_string())
            .bind(mode.value())
            .bind(&ids)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        tx.commit().await.map_err(db_err)
    }

    async fn reset_season(
        &self,
        season_id: Uuid,
        episode_ids: &[Uuid],
        modes: &[AnalysisMode],
    ) -> Result<(), ServiceError> {
        if episode_ids.is_empty() || modes.is_empty() {
            return Ok(());
        }
        let modes = serde_json::to_string(&modes.iter().map(|m| m.value()).collect::<Vec<_>>())
            .map_err(|e| ServiceError::backend(e.to_string()))?;
        // One transaction, so the episodes are re-analysed rather than left
        // marked analysed without their segments.
        let mut tx = self.db.writer().begin().await.map_err(db_err)?;
        sqlx::query(
            r#"DELETE FROM "FerrofinIntroSkipperSegments"
               WHERE "ItemId" IN (SELECT value FROM json_each(?1))
                 AND "Type" IN (SELECT value FROM json_each(?2)) AND "IsUserProvided" = 0"#,
        )
        .bind(json_ids(episode_ids)?)
        .bind(&modes)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            r#"UPDATE "FerrofinIntroSkipperSeasonStates" SET "EpisodeIds" = '[]'
               WHERE "SeasonId" = ?1 AND "Type" IN (SELECT value FROM json_each(?2))"#,
        )
        .bind(season_id.to_string())
        .bind(&modes)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        tx.commit().await.map_err(db_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_db;

    #[tokio::test]
    async fn actions_persist_per_season_and_mode() {
        let store = FerrofinIntroSkipperStore::new(test_db().await);
        let season = Uuid::from_u128(7);
        let other = Uuid::from_u128(8);
        assert!(store.analyzer_actions(season).await.unwrap().is_empty());
        store
            .set_analyzer_actions(
                season,
                &[
                    (AnalysisMode::Introduction, AnalyzerAction::None),
                    (AnalysisMode::Credits, AnalyzerAction::BlackFrame),
                ],
            )
            .await
            .unwrap();
        // Replaces only the named mode.
        store
            .set_analyzer_actions(
                season,
                &[(AnalysisMode::Credits, AnalyzerAction::Chromaprint)],
            )
            .await
            .unwrap();
        let actions = store.analyzer_actions(season).await.unwrap();
        assert_eq!(actions.len(), 2);
        assert_eq!(actions[&AnalysisMode::Introduction], AnalyzerAction::None);
        assert_eq!(actions[&AnalysisMode::Credits], AnalyzerAction::Chromaprint);
        assert!(store.analyzer_actions(other).await.unwrap().is_empty());

        store
            .set_analyzer_actions(other, &[(AnalysisMode::Recap, AnalyzerAction::Chapter)])
            .await
            .unwrap();
        store.retain_seasons(&[other]).await.unwrap();
        assert!(store.analyzer_actions(season).await.unwrap().is_empty());
        assert_eq!(store.analyzer_actions(other).await.unwrap().len(), 1);
    }

    fn seg(item: u128, mode: AnalysisMode, start: f64, end: f64, user: bool) -> StoredSegment {
        StoredSegment {
            item_id: Uuid::from_u128(item),
            mode,
            start,
            end,
            is_user_provided: user,
            config_hash: if user { String::new() } else { "h1".to_owned() },
        }
    }

    #[tokio::test]
    async fn segments_follow_update_timestamp_async() {
        use AnalysisMode::{Commercial, Credits, Introduction};
        let store = FerrofinIntroSkipperStore::new(test_db().await);
        let item = Uuid::from_u128(1);
        assert!(
            store
                .update_timestamp(seg(1, Introduction, 0.0, 30.0, true))
                .await
                .unwrap()
        );
        // Analysis cannot replace the user's intro, nor add credits over it.
        assert!(
            !store
                .update_timestamp(seg(1, Introduction, 5.0, 35.0, false))
                .await
                .unwrap()
        );
        assert!(
            !store
                .update_timestamp(seg(1, Credits, 20.0, 60.0, false))
                .await
                .unwrap()
        );
        assert!(
            store
                .update_timestamp(seg(1, Credits, 1300.0, 1400.0, false))
                .await
                .unwrap()
        );
        assert!(
            store
                .update_timestamp(seg(1, Credits, 1310.0, 1400.0, false))
                .await
                .unwrap()
        );
        assert!(
            store
                .update_timestamp(seg(1, Commercial, 600.0, 630.0, false))
                .await
                .unwrap()
        );
        assert!(
            store
                .update_timestamp(seg(1, Commercial, 900.0, 930.0, false))
                .await
                .unwrap()
        );
        assert!(
            !store
                .update_timestamp(seg(1, Commercial, 600.0, 630.0005, false))
                .await
                .unwrap()
        );
        let mut rows = store.segments(item).await.unwrap();
        rows.sort_by(|a, b| a.start.total_cmp(&b.start));
        let shape: Vec<_> = rows
            .iter()
            .map(|s| (s.mode, s.start, s.is_user_provided))
            .collect();
        assert_eq!(
            shape,
            [
                (Introduction, 0.0, true),
                (Commercial, 600.0, false),
                (Commercial, 900.0, false),
                (Credits, 1310.0, false),
            ]
        );
        assert_eq!(rows[3].config_hash, "h1");

        // Excluding the episode hides it from output, not from the store.
        let season = Uuid::from_u128(9);
        store.set_excluded(season, item, true).await.unwrap();
        assert_eq!(
            store.excluded_episodes(season).await.unwrap(),
            HashSet::from([item])
        );
        assert!(
            store
                .segments_unless_excluded(item)
                .await
                .unwrap()
                .is_empty()
        );
        store.retain_seasons(&[]).await.unwrap();
        assert_eq!(store.segments_unless_excluded(item).await.unwrap().len(), 4);

        // Deletes: one Commercial by range, a whole mode, whole items.
        assert_eq!(
            store
                .delete_timestamp(item, Commercial, None)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store
                .delete_timestamp(item, Commercial, Some((900.0, 930.0)))
                .await
                .unwrap(),
            1
        );
        assert_eq!(store.delete_mode(Credits).await.unwrap(), 1);
        assert_eq!(store.stored_item_ids().await.unwrap(), [item]);
        assert_eq!(store.delete_items(&[item]).await.unwrap(), 2);
        assert!(store.segments(item).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn season_state_tracks_analysed_episodes() {
        use AnalysisMode::{Credits, Introduction};
        let store = FerrofinIntroSkipperStore::new(test_db().await);
        let season = Uuid::from_u128(9);
        let (a, b, c) = (Uuid::from_u128(1), Uuid::from_u128(2), Uuid::from_u128(3));
        store
            .set_analyzer_actions(season, &[(Credits, AnalyzerAction::BlackFrame)])
            .await
            .unwrap();
        store
            .set_episode_ids(season, Introduction, &[a, b, c], "H1")
            .await
            .unwrap();
        store
            .set_episode_ids(season, Credits, &[a, b], "H2")
            .await
            .unwrap();
        let states = store.season_states(season).await.unwrap();
        assert_eq!(states[&Introduction].episode_ids, HashSet::from([a, b, c]));
        assert_eq!(states[&Introduction].config_hash, "H1");
        assert_eq!(
            states[&Credits].action,
            AnalyzerAction::BlackFrame,
            "the action is kept"
        );
        // One id from one mode, then from every mode.
        store
            .remove_episode_ids(Some(season), Some(Introduction), &[c])
            .await
            .unwrap();
        store.remove_episode_ids(None, None, &[a]).await.unwrap();
        let states = store.season_states(season).await.unwrap();
        assert_eq!(states[&Introduction].episode_ids, HashSet::from([b]));
        assert_eq!(states[&Credits].episode_ids, HashSet::from([b]));
        store
            .record_settled(season, &[Introduction], &[a, b])
            .await
            .unwrap();
        store
            .clear_episode_ids(season, Some(&[Credits]))
            .await
            .unwrap();
        let states = store.season_states(season).await.unwrap();
        assert!(states[&Credits].episode_ids.is_empty());
        assert_eq!(
            states[&Introduction].settled_episode_ids,
            HashSet::from([a, b])
        );

        // A stale automatic segment goes, a user's and a current one stay.
        store
            .update_timestamp(seg(1, Introduction, 0.0, 30.0, false))
            .await
            .unwrap();
        store
            .update_timestamp(seg(2, Introduction, 0.0, 30.0, true))
            .await
            .unwrap();
        store
            .update_timestamp(seg(3, Credits, 900.0, 960.0, false))
            .await
            .unwrap();
        assert_eq!(
            store
                .clean_stale_automatic(&[a, b], Introduction, "H9")
                .await
                .unwrap(),
            1
        );
        assert_eq!(
            store
                .clean_stale_automatic(&[c], Credits, "h1")
                .await
                .unwrap(),
            0
        );
        // A reset deletes the automatic segments of its modes and empties
        // their lists, together.
        store
            .set_episode_ids(season, Credits, &[c], "h1")
            .await
            .unwrap();
        store
            .reset_season(season, &[b, c], &[Credits])
            .await
            .unwrap();
        assert!(store.segments(c).await.unwrap().is_empty());
        assert_eq!(
            store.segments(b).await.unwrap().len(),
            1,
            "user-provided stays"
        );
        assert!(
            store.season_states(season).await.unwrap()[&Credits]
                .episode_ids
                .is_empty()
        );
    }

    #[tokio::test]
    async fn segments_of_reads_many_items_at_once() {
        use AnalysisMode::{Credits, Introduction};
        let store = FerrofinIntroSkipperStore::new(test_db().await);
        store
            .update_timestamp(seg(1, Introduction, 0.0, 30.0, false))
            .await
            .unwrap();
        store
            .update_timestamp(seg(1, Credits, 900.0, 960.0, false))
            .await
            .unwrap();
        store
            .update_timestamp(seg(2, Introduction, 0.0, 30.0, false))
            .await
            .unwrap();
        let by_item = store
            .segments_of(&[Uuid::from_u128(1), Uuid::from_u128(77)])
            .await
            .unwrap();
        assert_eq!(by_item.len(), 1);
        assert_eq!(by_item[&Uuid::from_u128(1)].len(), 2);
    }

    #[tokio::test]
    async fn a_rebuild_keeps_only_valid_segments() {
        use AnalysisMode::{Credits, Introduction};
        let store = FerrofinIntroSkipperStore::new(test_db().await);
        // `RebuildDatabaseAsync` keeps only valid (`End > 0`) segments.
        store
            .update_timestamp(seg(2, Introduction, 0.0, 0.0, true))
            .await
            .unwrap();
        store
            .update_timestamp(seg(2, Credits, 100.0, 130.0, true))
            .await
            .unwrap();
        assert_eq!(store.prune_invalid().await.unwrap(), 1);
        assert_eq!(store.segments(Uuid::from_u128(2)).await.unwrap().len(), 1);
    }
}
