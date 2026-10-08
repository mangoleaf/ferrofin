//! The Intro Skipper's per-season analyzer actions — the seam between the
//! `/Intros/AnalyzerActions/*` routes, the detection task and their store.
//!
//! Port of the plugin's `Plugin.SetAnalyzerActionAsync` /
//! `GetAllAnalyzerActionsAsync` / `GetAnalyzerActionAsync` (intro-skipper
//! `db09359`). The trait is object-safe and carries a `_assert_object_safe_*`
//! assertion.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use ferrofin_model::intro_skipper::{AnalysisMode, AnalyzerAction};
use uuid::Uuid;

use crate::error::ServiceError;

/// Persists the analyzer action a season uses for each analysis mode.
#[async_trait]
pub trait IntroSkipperStore: Send + Sync {
    /// The season's stored actions; a mode with none is absent (the caller
    /// treats it as [`AnalyzerAction::Default`], as upstream does).
    async fn analyzer_actions(
        &self,
        season_id: Uuid,
    ) -> Result<HashMap<AnalysisMode, AnalyzerAction>, ServiceError>;

    /// Stores `actions` for the season, replacing each named mode's action and
    /// leaving the others as they are (`SetAnalyzerActionAsync`).
    async fn set_analyzer_actions(
        &self,
        season_id: Uuid,
        actions: &[(AnalysisMode, AnalyzerAction)],
    ) -> Result<(), ServiceError>;

    /// Drops the actions of every season not in `season_ids` — the seasons
    /// that still have episodes (`CleanSeasonStateAsync`).
    async fn retain_seasons(&self, season_ids: &[Uuid]) -> Result<(), ServiceError>;
}

fn _assert_object_safe_intro_skipper_store(_: &dyn IntroSkipperStore) {}

/// A process-local [`IntroSkipperStore`]: the default of a state built without
/// a database (tests), never the server's.
#[derive(Debug, Default)]
pub struct InMemoryIntroSkipperStore(Mutex<HashMap<(Uuid, AnalysisMode), AnalyzerAction>>);

#[async_trait]
impl IntroSkipperStore for InMemoryIntroSkipperStore {
    async fn analyzer_actions(
        &self,
        season_id: Uuid,
    ) -> Result<HashMap<AnalysisMode, AnalyzerAction>, ServiceError> {
        let map = self
            .0
            .lock()
            .map_err(|_| ServiceError::backend("intro skipper store poisoned"))?;
        Ok(map
            .iter()
            .filter(|((season, _), _)| *season == season_id)
            .map(|((_, mode), action)| (*mode, *action))
            .collect())
    }

    async fn set_analyzer_actions(
        &self,
        season_id: Uuid,
        actions: &[(AnalysisMode, AnalyzerAction)],
    ) -> Result<(), ServiceError> {
        let mut map = self
            .0
            .lock()
            .map_err(|_| ServiceError::backend("intro skipper store poisoned"))?;
        for (mode, action) in actions {
            map.insert((season_id, *mode), *action);
        }
        Ok(())
    }

    async fn retain_seasons(&self, season_ids: &[Uuid]) -> Result<(), ServiceError> {
        self.0
            .lock()
            .map_err(|_| ServiceError::backend("intro skipper store poisoned"))?
            .retain(|(season, _), _| season_ids.contains(season));
        Ok(())
    }
}
