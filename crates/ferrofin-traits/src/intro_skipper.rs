//! The Intro Skipper's own store — the seam between the plugin's routes, the
//! detection task and the plugin's segment tier.
//!
//! Port of the plugin's database API (intro-skipper `db09359`, `Plugin.cs`):
//! per-season analyzer actions (`SetAnalyzerActionAsync` /
//! `GetAllAnalyzerActionsAsync`), detected and user-provided segments
//! (`UpdateTimestampAsync`, `GetSegmentsAsync`, `DeleteTimestamp(s)Async`), and
//! episodes excluded from media-segment output (`DbDisabledEpisode`). Segments
//! reach Jellyfin's `MediaSegments` only through [`refresh`], the port of the
//! plugin's `SegmentProvider` + `MediaSegmentRefreshService`. The trait is
//! object-safe and carries a `_assert_object_safe_*` assertion.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use async_trait::async_trait;
use ferrofin_model::intro_skipper::{AnalysisMode, AnalyzerAction};
use ferrofin_model::media_segments::{MediaSegmentDto, MediaSegmentType};
use uuid::Uuid;

use crate::error::ServiceError;
use crate::media_segments::MediaSegmentManager;

/// The plugin's name (`Plugin.ProviderName`), from which Jellyfin derives its
/// media-segment provider id.
pub const PROVIDER_NAME: &str = "Intro Skipper";

/// Two segment bounds closer than this (seconds) are the same bound
/// (`Plugin.SegmentComparisonEpsilon`).
pub const SEGMENT_COMPARISON_EPSILON: f64 = 0.001;

/// The plugin's media-segment provider id: Jellyfin's
/// `MediaSegmentManager.GetProviderId(name)` — the lowercased name's .NET
/// `GetMD5`, `"N"` format (`b0338b450421c081992860f1d02f261f`).
#[must_use]
pub fn provider_id() -> String {
    ferrofin_common::extensions::get_md5(&PROVIDER_NAME.to_lowercase())
        .simple()
        .to_string()
}

/// One row of the plugin's segment tier (`DbSegment`).
#[derive(Debug, Clone, PartialEq)]
pub struct StoredSegment {
    /// The episode or movie.
    pub item_id: Uuid,
    /// The analysis mode it was detected or entered for.
    pub mode: AnalysisMode,
    /// Start, in seconds.
    pub start: f64,
    /// End, in seconds.
    pub end: f64,
    /// Entered by a user (never overwritten by analysis).
    pub is_user_provided: bool,
    /// The configuration hash the analysis ran under (empty for user input).
    pub config_hash: String,
}

/// One mode's state for a season (`DbSeasonState`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SeasonState {
    /// The season's analyzer action for the mode.
    pub action: AnalyzerAction,
    /// The episodes the mode has analysed (with or without a result).
    pub episode_ids: HashSet<Uuid>,
    /// The configuration hash they were analysed under.
    pub config_hash: String,
    /// The episodes the last settled-season reanalysis covered.
    pub settled_episode_ids: HashSet<Uuid>,
}

/// What `UpdateTimestampAsync` does with a new segment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDecision {
    /// Keep the store as it is.
    Skip,
    /// Add the segment (a Commercial alongside the others).
    Insert,
    /// Replace the item's segments of this mode with it.
    Replace,
}

/// `Plugin.UpdateTimestampAsync`'s rules, given every stored segment of the
/// item: a Commercial is added unless an equal one exists; otherwise an
/// analysis result never replaces a user-provided segment, and analysed
/// credits that overlap the stored introduction are dropped.
#[must_use]
pub fn decide_update(
    existing: &[StoredSegment],
    mode: AnalysisMode,
    start: f64,
    end: f64,
    is_user_provided: bool,
) -> UpdateDecision {
    let same = |s: &&StoredSegment| s.mode == mode;
    if mode == AnalysisMode::Commercial {
        let duplicate = existing.iter().filter(same).any(|s| {
            (s.start - start).abs() <= SEGMENT_COMPARISON_EPSILON
                && (s.end - end).abs() <= SEGMENT_COMPARISON_EPSILON
        });
        return if duplicate {
            UpdateDecision::Skip
        } else {
            UpdateDecision::Insert
        };
    }
    if !is_user_provided && existing.iter().filter(same).any(|s| s.is_user_provided) {
        return UpdateDecision::Skip;
    }
    if mode == AnalysisMode::Credits
        && !is_user_provided
        && let Some(intro) = existing
            .iter()
            .find(|s| s.mode == AnalysisMode::Introduction)
        && start < intro.end
        && intro.start < end
    {
        return UpdateDecision::Skip;
    }
    UpdateDecision::Replace
}

/// `Plugin.GetTimestampsAsync`: per mode, the earliest-starting segment.
#[must_use]
pub fn timestamps(segments: &[StoredSegment]) -> HashMap<AnalysisMode, StoredSegment> {
    let mut by_mode: HashMap<AnalysisMode, StoredSegment> = HashMap::new();
    for segment in segments {
        by_mode
            .entry(segment.mode)
            .and_modify(|kept| {
                if segment.start < kept.start {
                    *kept = segment.clone();
                }
            })
            .or_insert_with(|| segment.clone());
    }
    by_mode
}

/// The media-segment type a mode publishes as (`SegmentProvider._segmentMappings`).
#[must_use]
pub fn segment_type(mode: AnalysisMode) -> Option<MediaSegmentType> {
    match mode {
        AnalysisMode::Introduction => Some(MediaSegmentType::Intro),
        AnalysisMode::Credits => Some(MediaSegmentType::Outro),
        AnalysisMode::Preview => Some(MediaSegmentType::Preview),
        AnalysisMode::Recap => Some(MediaSegmentType::Recap),
        AnalysisMode::Commercial => Some(MediaSegmentType::Commercial),
        AnalysisMode::Unrecognized(_) => None,
    }
}

/// The mode a media-segment type was entered as (`Plugin.MapSegmentTypeToMode`).
#[must_use]
pub fn segment_mode(kind: MediaSegmentType) -> Option<AnalysisMode> {
    match kind {
        MediaSegmentType::Intro => Some(AnalysisMode::Introduction),
        MediaSegmentType::Outro => Some(AnalysisMode::Credits),
        MediaSegmentType::Preview => Some(AnalysisMode::Preview),
        MediaSegmentType::Recap => Some(AnalysisMode::Recap),
        MediaSegmentType::Commercial => Some(AnalysisMode::Commercial),
        MediaSegmentType::Unknown | MediaSegmentType::Unrecognized(_) => None,
    }
}

/// `SegmentProvider.GetMediaSegments`: in start order, every segment with a
/// positive end, one per mode except Commercial, as media segments.
#[must_use]
pub fn published(item_id: Uuid, segments: &[StoredSegment]) -> Vec<MediaSegmentDto> {
    let mut ordered: Vec<&StoredSegment> = segments.iter().collect();
    ordered.sort_by(|a, b| a.start.total_cmp(&b.start));
    let mut seen = HashSet::new();
    ordered
        .into_iter()
        .filter_map(|segment| {
            let kind = segment_type(segment.mode)?;
            if segment.end <= 0.0
                || (segment.mode != AnalysisMode::Commercial && !seen.insert(segment.mode))
            {
                return None;
            }
            Some(MediaSegmentDto {
                id: Uuid::nil(),
                item_id,
                type_: kind,
                start_ticks: seconds_to_ticks(segment.start),
                end_ticks: seconds_to_ticks(segment.end),
            })
        })
        .collect()
}

/// `(long)(seconds * TimeSpan.TicksPerSecond)`.
#[allow(clippy::cast_possible_truncation)]
fn seconds_to_ticks(seconds: f64) -> i64 {
    (seconds * 10_000_000.0) as i64
}

/// `MediaSegmentRefreshService.RefreshAsync`: replaces the plugin's published
/// segments of `item_id` with what the store holds (none for an excluded
/// episode). Upstream's `RunSegmentPluginProviders(overwrite)` deletes every
/// provider's rows of the item and re-runs the external providers; Ferrofin's
/// other segment writers (WASM plugins) manage their own rows, so only the
/// plugin's rows are replaced. The delete and inserts are not one transaction
/// (nor are upstream's); a racing refresh is repaired by the next one.
///
/// # Errors
/// A store or media-segment failure.
pub async fn refresh(
    store: &dyn IntroSkipperStore,
    media_segments: &dyn MediaSegmentManager,
    item_id: Uuid,
) -> Result<(), ServiceError> {
    let provider = provider_id();
    let segments = store.segments_unless_excluded(item_id).await?;
    media_segments
        .delete_provider_segments(item_id, &provider, None)
        .await?;
    for segment in published(item_id, &segments) {
        media_segments.create_segment(&segment, &provider).await?;
    }
    Ok(())
}

/// `MediaSegmentRefreshService.RemoveIntroSkipperSegmentsAsync`: drops the
/// plugin's published segments of `item_id`. (Upstream deletes every
/// provider's rows and re-runs the others; Ferrofin has no other segment
/// provider to re-run, so only the plugin's rows go.)
///
/// # Errors
/// A media-segment failure.
pub async fn remove_published(
    media_segments: &dyn MediaSegmentManager,
    item_id: Uuid,
) -> Result<(), ServiceError> {
    media_segments
        .delete_provider_segments(item_id, &provider_id(), None)
        .await
}

/// Persists the plugin's per-season and per-item state.
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

    /// Drops the season state and disabled episodes of every season not in
    /// `season_ids` — the seasons that still have episodes
    /// (`CleanSeasonStateAsync`).
    async fn retain_seasons(&self, season_ids: &[Uuid]) -> Result<(), ServiceError>;

    /// Stores a segment under [`decide_update`]'s rules
    /// (`UpdateTimestampAsync`); whether it was stored.
    async fn update_timestamp(&self, segment: StoredSegment) -> Result<bool, ServiceError>;

    /// Every stored segment of the item (`GetSegmentsAsync`).
    async fn segments(&self, item_id: Uuid) -> Result<Vec<StoredSegment>, ServiceError>;

    /// Every stored segment of the items, by item, in one read (the segment
    /// half of `GetSeasonQueueSnapshotAsync`); items without one are absent.
    async fn segments_of(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<StoredSegment>>, ServiceError>;

    /// The item's segments, or none when it is a disabled episode
    /// (`GetSegmentsUnlessExcludedAsync`).
    async fn segments_unless_excluded(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<StoredSegment>, ServiceError>;

    /// Deletes the item's segments of `mode`, or only the one matching `range`
    /// (`DeleteTimestampAsync`; a Commercial needs a range); how many went.
    async fn delete_timestamp(
        &self,
        item_id: Uuid,
        mode: AnalysisMode,
        range: Option<(f64, f64)>,
    ) -> Result<u64, ServiceError>;

    /// Deletes every segment of `mode`, on every item (`ResetIntroTimestamps`);
    /// how many went.
    async fn delete_mode(&self, mode: AnalysisMode) -> Result<u64, ServiceError>;

    /// Deletes every segment of the items (`DeleteTimestampsAsync`); how many
    /// went.
    async fn delete_items(&self, item_ids: &[Uuid]) -> Result<u64, ServiceError>;

    /// The season's episodes excluded from media-segment output
    /// (`GetMediaSegmentExcludedEpisodeIdsAsync`).
    async fn excluded_episodes(&self, season_id: Uuid) -> Result<HashSet<Uuid>, ServiceError>;

    /// Excludes (or re-includes) an episode (`SetMediaSegmentExcludedAsync`).
    async fn set_excluded(
        &self,
        season_id: Uuid,
        episode_id: Uuid,
        excluded: bool,
    ) -> Result<(), ServiceError>;

    /// Every item with a stored segment.
    async fn stored_item_ids(&self) -> Result<Vec<Uuid>, ServiceError>;

    /// The season's state per mode (modes never touched are absent).
    async fn season_states(
        &self,
        season_id: Uuid,
    ) -> Result<HashMap<AnalysisMode, SeasonState>, ServiceError>;

    /// Records the episodes `mode` analysed and the hash they were analysed
    /// under, keeping the action (`SetEpisodeIdsAsync`).
    async fn set_episode_ids(
        &self,
        season_id: Uuid,
        mode: AnalysisMode,
        episode_ids: &[Uuid],
        config_hash: &str,
    ) -> Result<(), ServiceError>;

    /// Drops `episode_ids` from the season's analysed lists — of `mode`, or of
    /// every mode (`RemoveEpisodeIdAsync`, `ClearExcludedTimestampsAsync`);
    /// every season when `season_id` is `None`.
    async fn remove_episode_ids(
        &self,
        season_id: Option<Uuid>,
        mode: Option<AnalysisMode>,
        episode_ids: &[Uuid],
    ) -> Result<(), ServiceError>;

    /// Empties the season's analysed lists, of `modes` or of every mode
    /// (`EraseSeasonAsync`).
    async fn clear_episode_ids(
        &self,
        season_id: Uuid,
        modes: Option<&[AnalysisMode]>,
    ) -> Result<(), ServiceError>;

    /// Deletes the items' automatic `mode` segments analysed under another
    /// hash (`CleanStaleAutomaticSegmentsAsync`); user-provided ones stay.
    async fn clean_stale_automatic(
        &self,
        item_ids: &[Uuid],
        mode: AnalysisMode,
        config_hash: &str,
    ) -> Result<u64, ServiceError>;

    /// Records that a settled-season reanalysis of `modes` covered
    /// `episode_ids` (`RecordSettleReanalysisAsync`).
    async fn record_settled(
        &self,
        season_id: Uuid,
        modes: &[AnalysisMode],
        episode_ids: &[Uuid],
    ) -> Result<(), ServiceError>;

    /// Deletes the items' automatic segments of `modes` and empties those
    /// modes' analysed lists, together (`ResetSeasonForReanalysisAsync`).
    async fn reset_season(
        &self,
        season_id: Uuid,
        episode_ids: &[Uuid],
        modes: &[AnalysisMode],
    ) -> Result<(), ServiceError>;

    /// Deletes every segment that is not valid (`End <= 0`) — what upstream's
    /// `RebuildDatabaseAsync` leaves behind when it restores its backup; how
    /// many went.
    async fn prune_invalid(&self) -> Result<u64, ServiceError>;
}

fn _assert_object_safe_intro_skipper_store(_: &dyn IntroSkipperStore) {}

/// The plugin's analysis runtime: its one-pass-at-a-time lock
/// (`ScheduledTaskSemaphore`), on-demand season rescans and the detection
/// cache — implemented by the Intro Skipper extension, so the routes reach
/// them without depending on it.
#[async_trait]
pub trait IntroSkipperAnalysis: Send + Sync {
    /// Whether an analysis pass is running (any of them: the scheduled
    /// detection, the Media Segment Scan or a rescan).
    fn is_running(&self) -> bool;

    /// Starts, in the background, erasing the season (or movie) — segments
    /// and cache — then analysing only it (`VisualizationController.ScanSeason`).
    /// `false` when a pass is already running.
    async fn rescan(&self, season_id: Uuid) -> Result<bool, ServiceError>;

    /// Deletes cached analysis data of `items` (every item when `None`) for
    /// `mode` (every mode when `None`) — the plugin's `DeleteForItem` /
    /// `DeleteByMode`; how many entries went.
    async fn erase_cache(
        &self,
        items: Option<&[Uuid]>,
        mode: Option<AnalysisMode>,
    ) -> Result<u64, ServiceError>;

    /// Deletes the segments and cache of every item the exclusion policy now
    /// matches, then republishes them (`ClearExcludedTimestampsAsync`).
    async fn clear_excluded(&self) -> Result<ExcludedClear, ServiceError>;

    /// Items were added, updated or removed (the plugin `Entrypoint`'s
    /// `ItemAdded`/`ItemUpdated`/`ItemRemoved`): their seasons are queued for
    /// automatic analysis, a removed item's cache deleted.
    async fn items_changed(&self, added: &[Uuid], updated: &[Uuid], removed: &[Uuid]);

    /// A scheduled task finished (`ITaskManager.TaskCompleted`); `completed`
    /// when it succeeded.
    async fn task_completed(&self, key: &str, completed: bool);

    /// A plugin's configuration was saved (`ConfigurationChanged`).
    async fn plugin_configuration_changed(&self, plugin_id: Uuid);

    /// Whether `GET /MediaSegments/{itemId}` hides the item's Intro segments
    /// (`MediaSegmentsFirstEpisodeFilter`: a season's first episode, with
    /// `SkipFirstEpisode`).
    async fn hides_intros(&self, item_id: Uuid) -> bool;
}

fn _assert_object_safe_intro_skipper_analysis(_: &dyn IntroSkipperAnalysis) {}

/// What clearing the excluded items' data removed
/// (`ClearExcludedTimestampsResponse`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExcludedClear {
    /// The excluded items found.
    pub affected_items: u64,
    /// Their stored segments deleted.
    pub removed_segments: u64,
    /// Their cache entries deleted.
    pub removed_cache_entries: u64,
}

/// An [`IntroSkipperAnalysis`] with nothing behind it: the default of a state
/// built without the extension (tests), never the server's — it is never
/// running, and a rescan or cache erase is a backend error.
#[derive(Debug, Default)]
pub struct DetachedIntroSkipperAnalysis;

#[async_trait]
impl IntroSkipperAnalysis for DetachedIntroSkipperAnalysis {
    fn is_running(&self) -> bool {
        false
    }

    async fn rescan(&self, _season_id: Uuid) -> Result<bool, ServiceError> {
        Err(ServiceError::backend(
            "the intro skipper extension is not attached",
        ))
    }

    async fn erase_cache(
        &self,
        _items: Option<&[Uuid]>,
        _mode: Option<AnalysisMode>,
    ) -> Result<u64, ServiceError> {
        Err(ServiceError::backend(
            "the intro skipper extension is not attached",
        ))
    }

    async fn clear_excluded(&self) -> Result<ExcludedClear, ServiceError> {
        Err(ServiceError::backend(
            "the intro skipper extension is not attached",
        ))
    }

    // Without the extension there is nothing to analyse.
    async fn items_changed(&self, _added: &[Uuid], _updated: &[Uuid], _removed: &[Uuid]) {}

    async fn task_completed(&self, _key: &str, _completed: bool) {}

    async fn plugin_configuration_changed(&self, _plugin_id: Uuid) {}

    async fn hides_intros(&self, _item_id: Uuid) -> bool {
        false
    }
}

/// The [`InMemoryIntroSkipperStore`] state.
#[derive(Debug, Default)]
struct Memory {
    states: HashMap<(Uuid, AnalysisMode), SeasonState>,
    segments: Vec<StoredSegment>,
    disabled: HashSet<(Uuid, Uuid)>,
    read_only: bool,
}

/// A process-local [`IntroSkipperStore`]: the default of a state built without
/// a database (tests), never the server's.
#[derive(Debug, Default)]
pub struct InMemoryIntroSkipperStore(Mutex<Memory>);

impl InMemoryIntroSkipperStore {
    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Memory>, ServiceError> {
        self.0
            .lock()
            .map_err(|_| ServiceError::backend("intro skipper store poisoned"))
    }

    /// Makes segment writes fail (or succeed again), as a full disk or a
    /// locked database would — for tests of the failure paths.
    pub fn set_read_only(&self, read_only: bool) {
        if let Ok(mut memory) = self.0.lock() {
            memory.read_only = read_only;
        }
    }
}

#[async_trait]
impl IntroSkipperStore for InMemoryIntroSkipperStore {
    async fn analyzer_actions(
        &self,
        season_id: Uuid,
    ) -> Result<HashMap<AnalysisMode, AnalyzerAction>, ServiceError> {
        Ok(self
            .lock()?
            .states
            .iter()
            .filter(|((season, _), _)| *season == season_id)
            .map(|((_, mode), state)| (*mode, state.action))
            .collect())
    }

    async fn set_analyzer_actions(
        &self,
        season_id: Uuid,
        actions: &[(AnalysisMode, AnalyzerAction)],
    ) -> Result<(), ServiceError> {
        let mut memory = self.lock()?;
        for (mode, action) in actions {
            memory.states.entry((season_id, *mode)).or_default().action = *action;
        }
        Ok(())
    }

    async fn retain_seasons(&self, season_ids: &[Uuid]) -> Result<(), ServiceError> {
        let mut memory = self.lock()?;
        memory
            .states
            .retain(|(season, _), _| season_ids.contains(season));
        memory
            .disabled
            .retain(|(season, _)| season_ids.contains(season));
        Ok(())
    }

    async fn update_timestamp(&self, segment: StoredSegment) -> Result<bool, ServiceError> {
        let mut memory = self.lock()?;
        if memory.read_only {
            return Err(ServiceError::backend("intro skipper store is read-only"));
        }
        let existing: Vec<StoredSegment> = memory
            .segments
            .iter()
            .filter(|s| s.item_id == segment.item_id)
            .cloned()
            .collect();
        match decide_update(
            &existing,
            segment.mode,
            segment.start,
            segment.end,
            segment.is_user_provided,
        ) {
            UpdateDecision::Skip => return Ok(false),
            UpdateDecision::Replace => memory
                .segments
                .retain(|s| !(s.item_id == segment.item_id && s.mode == segment.mode)),
            UpdateDecision::Insert => {}
        }
        memory.segments.push(segment);
        Ok(true)
    }

    async fn segments(&self, item_id: Uuid) -> Result<Vec<StoredSegment>, ServiceError> {
        Ok(self
            .lock()?
            .segments
            .iter()
            .filter(|s| s.item_id == item_id)
            .cloned()
            .collect())
    }

    async fn segments_of(
        &self,
        item_ids: &[Uuid],
    ) -> Result<HashMap<Uuid, Vec<StoredSegment>>, ServiceError> {
        let mut by_item: HashMap<Uuid, Vec<StoredSegment>> = HashMap::new();
        for segment in &self.lock()?.segments {
            if item_ids.contains(&segment.item_id) {
                by_item
                    .entry(segment.item_id)
                    .or_default()
                    .push(segment.clone());
            }
        }
        Ok(by_item)
    }

    async fn segments_unless_excluded(
        &self,
        item_id: Uuid,
    ) -> Result<Vec<StoredSegment>, ServiceError> {
        let memory = self.lock()?;
        if memory
            .disabled
            .iter()
            .any(|(_, episode)| *episode == item_id)
        {
            return Ok(Vec::new());
        }
        Ok(memory
            .segments
            .iter()
            .filter(|s| s.item_id == item_id)
            .cloned()
            .collect())
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
        let mut memory = self.lock()?;
        let before = memory.segments.len();
        memory.segments.retain(|s| {
            !(s.item_id == item_id
                && s.mode == mode
                && range.is_none_or(|(start, end)| {
                    (s.start - start).abs() <= SEGMENT_COMPARISON_EPSILON
                        && (s.end - end).abs() <= SEGMENT_COMPARISON_EPSILON
                }))
        });
        Ok((before - memory.segments.len()) as u64)
    }

    async fn delete_mode(&self, mode: AnalysisMode) -> Result<u64, ServiceError> {
        let mut memory = self.lock()?;
        let before = memory.segments.len();
        memory.segments.retain(|s| s.mode != mode);
        Ok((before - memory.segments.len()) as u64)
    }

    async fn delete_items(&self, item_ids: &[Uuid]) -> Result<u64, ServiceError> {
        let mut memory = self.lock()?;
        let before = memory.segments.len();
        memory.segments.retain(|s| !item_ids.contains(&s.item_id));
        Ok((before - memory.segments.len()) as u64)
    }

    async fn excluded_episodes(&self, season_id: Uuid) -> Result<HashSet<Uuid>, ServiceError> {
        Ok(self
            .lock()?
            .disabled
            .iter()
            .filter(|(season, _)| *season == season_id)
            .map(|(_, episode)| *episode)
            .collect())
    }

    async fn set_excluded(
        &self,
        season_id: Uuid,
        episode_id: Uuid,
        excluded: bool,
    ) -> Result<(), ServiceError> {
        let mut memory = self.lock()?;
        if excluded {
            memory.disabled.insert((season_id, episode_id));
        } else {
            memory.disabled.remove(&(season_id, episode_id));
        }
        Ok(())
    }

    async fn stored_item_ids(&self) -> Result<Vec<Uuid>, ServiceError> {
        let mut ids: Vec<Uuid> = self.lock()?.segments.iter().map(|s| s.item_id).collect();
        ids.sort_unstable();
        ids.dedup();
        Ok(ids)
    }

    async fn prune_invalid(&self) -> Result<u64, ServiceError> {
        let mut memory = self.lock()?;
        let before = memory.segments.len();
        memory.segments.retain(|s| s.end > 0.0);
        Ok((before - memory.segments.len()) as u64)
    }

    async fn season_states(
        &self,
        season_id: Uuid,
    ) -> Result<HashMap<AnalysisMode, SeasonState>, ServiceError> {
        Ok(self
            .lock()?
            .states
            .iter()
            .filter(|((season, _), _)| *season == season_id)
            .map(|((_, mode), state)| (*mode, state.clone()))
            .collect())
    }

    async fn set_episode_ids(
        &self,
        season_id: Uuid,
        mode: AnalysisMode,
        episode_ids: &[Uuid],
        config_hash: &str,
    ) -> Result<(), ServiceError> {
        let mut memory = self.lock()?;
        let state = memory.states.entry((season_id, mode)).or_default();
        state.episode_ids = episode_ids.iter().copied().collect();
        config_hash.clone_into(&mut state.config_hash);
        Ok(())
    }

    async fn remove_episode_ids(
        &self,
        season_id: Option<Uuid>,
        mode: Option<AnalysisMode>,
        episode_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        for ((season, state_mode), state) in &mut self.lock()?.states {
            if season_id.is_none_or(|s| s == *season) && mode.is_none_or(|m| m == *state_mode) {
                state.episode_ids.retain(|id| !episode_ids.contains(id));
            }
        }
        Ok(())
    }

    async fn clear_episode_ids(
        &self,
        season_id: Uuid,
        modes: Option<&[AnalysisMode]>,
    ) -> Result<(), ServiceError> {
        for ((season, mode), state) in &mut self.lock()?.states {
            if *season == season_id && modes.is_none_or(|m| m.contains(mode)) {
                state.episode_ids.clear();
            }
        }
        Ok(())
    }

    async fn clean_stale_automatic(
        &self,
        item_ids: &[Uuid],
        mode: AnalysisMode,
        config_hash: &str,
    ) -> Result<u64, ServiceError> {
        let mut memory = self.lock()?;
        let before = memory.segments.len();
        memory.segments.retain(|s| {
            !(item_ids.contains(&s.item_id)
                && s.mode == mode
                && !s.is_user_provided
                && s.config_hash != config_hash)
        });
        Ok((before - memory.segments.len()) as u64)
    }

    async fn record_settled(
        &self,
        season_id: Uuid,
        modes: &[AnalysisMode],
        episode_ids: &[Uuid],
    ) -> Result<(), ServiceError> {
        let mut memory = self.lock()?;
        for mode in modes {
            memory
                .states
                .entry((season_id, *mode))
                .or_default()
                .settled_episode_ids = episode_ids.iter().copied().collect();
        }
        Ok(())
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
        let mut memory = self.lock()?;
        memory.segments.retain(|s| {
            !(episode_ids.contains(&s.item_id) && modes.contains(&s.mode) && !s.is_user_provided)
        });
        for ((season, mode), state) in &mut memory.states {
            if *season == season_id && modes.contains(mode) {
                state.episode_ids.clear();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(mode: AnalysisMode, start: f64, end: f64, user: bool) -> StoredSegment {
        StoredSegment {
            item_id: Uuid::from_u128(1),
            mode,
            start,
            end,
            is_user_provided: user,
            config_hash: String::new(),
        }
    }

    #[test]
    fn provider_id_is_jellyfins() {
        assert_eq!(provider_id(), "b0338b450421c081992860f1d02f261f");
    }

    #[test]
    fn update_rules_follow_update_timestamp_async() {
        use AnalysisMode::{Commercial, Credits, Introduction};
        let user_intro = [seg(Introduction, 0.0, 30.0, true)];
        // Analysis never replaces a user-provided segment; the user can.
        assert_eq!(
            decide_update(&user_intro, Introduction, 1.0, 31.0, false),
            UpdateDecision::Skip
        );
        assert_eq!(
            decide_update(&user_intro, Introduction, 1.0, 31.0, true),
            UpdateDecision::Replace
        );
        // Analysed credits overlapping the intro are dropped; a user's are kept.
        assert_eq!(
            decide_update(&user_intro, Credits, 20.0, 60.0, false),
            UpdateDecision::Skip
        );
        assert_eq!(
            decide_update(&user_intro, Credits, 20.0, 60.0, true),
            UpdateDecision::Replace
        );
        assert_eq!(
            decide_update(&user_intro, Credits, 30.0, 60.0, false),
            UpdateDecision::Replace
        );
        // Commercials accumulate, except an equal one (within the epsilon).
        let ads = [seg(Commercial, 100.0, 130.0, false)];
        assert_eq!(
            decide_update(&ads, Commercial, 100.0005, 130.0, false),
            UpdateDecision::Skip
        );
        assert_eq!(
            decide_update(&ads, Commercial, 200.0, 230.0, false),
            UpdateDecision::Insert
        );
    }

    #[test]
    fn published_keeps_the_first_per_mode_and_every_commercial() {
        use AnalysisMode::{Commercial, Introduction, Recap};
        let item = Uuid::from_u128(1);
        let out = published(
            item,
            &[
                seg(Introduction, 50.0, 60.0, false),
                seg(Introduction, 10.0, 20.0, false),
                seg(Commercial, 300.0, 330.0, false),
                seg(Commercial, 100.0, 130.0, false),
                seg(Recap, 0.0, 0.0, false),
            ],
        );
        let shape: Vec<_> = out.iter().map(|s| (s.type_, s.start_ticks)).collect();
        assert_eq!(
            shape,
            [
                (MediaSegmentType::Intro, 100_000_000),
                (MediaSegmentType::Commercial, 1_000_000_000),
                (MediaSegmentType::Commercial, 3_000_000_000),
            ]
        );
    }

    #[tokio::test]
    async fn the_memory_store_honours_exclusion_and_deletes() {
        let store = InMemoryIntroSkipperStore::default();
        let (season, item) = (Uuid::from_u128(9), Uuid::from_u128(1));
        assert!(
            store
                .update_timestamp(seg(AnalysisMode::Introduction, 0.0, 30.0, false))
                .await
                .unwrap()
        );
        store.set_excluded(season, item, true).await.unwrap();
        assert!(
            store
                .segments_unless_excluded(item)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.segments(item).await.unwrap().len(), 1);
        store.set_excluded(season, item, false).await.unwrap();
        assert_eq!(store.segments_unless_excluded(item).await.unwrap().len(), 1);
        assert_eq!(
            store
                .delete_timestamp(item, AnalysisMode::Commercial, None)
                .await
                .unwrap(),
            0
        );
        assert_eq!(
            store.delete_mode(AnalysisMode::Introduction).await.unwrap(),
            1
        );
    }
}
