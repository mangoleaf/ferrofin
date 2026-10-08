//! The SQLite [`IntroSkipperStore`]: the Intro Skipper's per-season analyzer
//! actions in the Ferrofin-owned `FerrofinIntroSkipperAnalyzerActions` table
//! (migration 0036). Port of the plugin's `DbSeasonState.Action` reads and
//! writes (`Plugin.SetAnalyzerActionAsync`, `GetAllAnalyzerActionsAsync`).

use std::collections::HashMap;

use async_trait::async_trait;
use ferrofin_db::Database;
use ferrofin_model::intro_skipper::{AnalysisMode, AnalyzerAction};
use ferrofin_model::json::enums::JsonEnum as _;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::intro_skipper::IntroSkipperStore;
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

#[async_trait]
impl IntroSkipperStore for FerrofinIntroSkipperStore {
    async fn analyzer_actions(
        &self,
        season_id: Uuid,
    ) -> Result<HashMap<AnalysisMode, AnalyzerAction>, ServiceError> {
        let rows: Vec<(i32, i32)> = sqlx::query_as(
            r#"SELECT "Mode", "Action" FROM "FerrofinIntroSkipperAnalyzerActions"
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
                r#"INSERT INTO "FerrofinIntroSkipperAnalyzerActions" ("SeasonId", "Mode", "Action")
                   VALUES (?1, ?2, ?3)
                   ON CONFLICT ("SeasonId", "Mode") DO UPDATE SET "Action" = excluded."Action""#,
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
        let ids: Vec<String> = season_ids.iter().map(Uuid::to_string).collect();
        let ids = serde_json::to_string(&ids).map_err(|e| ServiceError::backend(e.to_string()))?;
        sqlx::query(
            r#"DELETE FROM "FerrofinIntroSkipperAnalyzerActions"
               WHERE "SeasonId" NOT IN (SELECT value FROM json_each(?1))"#,
        )
        .bind(ids)
        .execute(self.db.writer())
        .await
        .map_err(db_err)?;
        Ok(())
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
}
