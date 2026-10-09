//! The analysis engine — port of the plugin's `BaseItemAnalyzerTask`,
//! `QueueManager.VerifyQueueAsync`, `ChromaprintAnalyzer.AnalyzeMediaFiles`
//! and `TimeAdjustmentHelper` (intro-skipper `db09359`).
//!
//! Per queued season: each item's per-mode state is read back from its stored
//! segments and the season state (so an analysed episode is not analysed
//! again until the settings that shaped it change); a settled season can be
//! reset and analysed afresh; then every scanned mode runs its analyzer chain,
//! records the episodes it analysed under the configuration hash, and the
//! season's segments are published.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Utc};
use ferrofin_chromaprint::{AnalysisMode as PrintMode, CompareConfig, TimeRange, compare_episodes};
use ferrofin_core::TaskProgress;
use ferrofin_model::entities_media::ChapterInfo;
use ferrofin_model::intro_skipper::{AnalysisMode as Mode, AnalyzerAction};
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::intro_skipper::{self as intro_store, SeasonState, StoredSegment};
use futures_util::StreamExt as _;
use uuid::Uuid;

use super::queue::{Category, QueuedEpisode};
use super::{DetectSegmentsTask, IntroSkipperConfig, chapter_analyzer, config_hash};
use crate::ffmpeg::ProcessOptions;
use crate::fingerprint::Fingerprinter;

/// One millisecond, the helpers' float tolerance (`TimeAdjustmentHelper.Epsilon`).
const EPSILON: f64 = 1e-3;
/// A recap card's minimum match length (`ChromaprintAnalyzer.RecapCardMinimumDuration`).
const RECAP_CARD_MINIMUM: f64 = 3.0;
/// How close an anime preview must already be to be left alone
/// (`AnimePreviewStartTolerance`).
const ANIME_PREVIEW_TOLERANCE: f64 = 0.5;
/// A settled-season reanalysis needs at least this many episodes
/// (`SeasonReanalysisPlanner.MinimumEpisodes`).
const SETTLED_MINIMUM_EPISODES: usize = 3;
/// `PluginConfiguration.MaximumSettledSeasonDelayHours`.
const MAXIMUM_SETTLED_DELAY_HOURS: i32 = 87_600;

/// Where an item stands for one mode (`EpisodeState`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) enum EpisodeState {
    /// Needs analysis.
    #[default]
    NotAnalyzed,
    /// Has a stored result under the current settings.
    Analyzed,
    /// Was analysed under the current settings without a result.
    NoSegments,
    /// Has a user-provided segment (never re-analysed).
    UserProvided,
}

/// A verified queue entry with its per-mode states (`QueuedEpisode.IsAnalyzed`).
#[derive(Debug, Clone)]
pub(super) struct Item {
    /// The queue entry.
    pub entry: QueuedEpisode,
    states: [EpisodeState; 5],
    /// The hash the current mode is analysed under (`AnalysisConfigHash`).
    config_hash: String,
}

impl Item {
    fn new(entry: QueuedEpisode) -> Self {
        Self {
            entry,
            states: [EpisodeState::NotAnalyzed; 5],
            config_hash: String::new(),
        }
    }

    pub(super) fn get(&self, mode: Mode) -> EpisodeState {
        slot(mode).map_or(EpisodeState::NotAnalyzed, |i| self.states[i])
    }

    fn set(&mut self, mode: Mode, state: EpisodeState) {
        if let Some(i) = slot(mode) {
            self.states[i] = state;
        }
    }

    /// `NeedsAnalysis`: neither analysed nor user-provided.
    pub(super) fn needs_analysis(&self, mode: Mode) -> bool {
        !matches!(
            self.get(mode),
            EpisodeState::Analyzed | EpisodeState::UserProvided
        )
    }

    /// `GetFingerprintRange`: the intro window for intros and recaps, the
    /// credits window for credits.
    fn fingerprint_range(&self, mode: Mode) -> Option<(f64, f64)> {
        let e = &self.entry;
        match mode {
            Mode::Introduction | Mode::Recap => Some((0.0, e.intro_fingerprint_end)),
            Mode::Credits => Some((
                e.credits_fingerprint_start,
                if e.credits_fingerprint_end > 0.0 {
                    e.credits_fingerprint_end
                } else {
                    e.duration
                },
            )),
            _ => None,
        }
    }
}

/// The mode's slot in an item's state array.
fn slot(mode: Mode) -> Option<usize> {
    usize::try_from(mode.value()).ok().filter(|i| *i < 5)
}

/// The modes the configuration scans, in upstream's order.
pub(super) fn modes(c: &IntroSkipperConfig) -> Vec<Mode> {
    [
        (c.scan_introduction, Mode::Introduction),
        (c.scan_credits, Mode::Credits),
        (c.scan_recap, Mode::Recap),
        (c.scan_preview, Mode::Preview),
        (c.scan_commercial, Mode::Commercial),
    ]
    .into_iter()
    .filter_map(|(on, mode)| on.then_some(mode))
    .collect()
}

/// An analyzer of the chain (`IMediaFileAnalyzer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Analyzer {
    Chapter,
    BlackFrame,
    Chromaprint,
}

/// `AnalyzeItemsAsync`'s chain for `mode`: the chapter analyzer first, then
/// the mode's own, with the season's action (or `PreferChromaprint`)
/// promoting one to the front.
fn analyzer_chain(
    mode: Mode,
    category: Category,
    ffmpeg_valid: bool,
    action: AnalyzerAction,
    prefer_chromaprint: bool,
) -> Vec<Analyzer> {
    let movie = category == Category::Movie;
    let mut chain = vec![Analyzer::Chapter];
    match mode {
        Mode::Credits if category == Category::AnimeEpisode => {
            if ffmpeg_valid {
                chain.push(Analyzer::Chromaprint);
            }
            chain.push(Analyzer::BlackFrame);
        }
        Mode::Credits => {
            chain.push(Analyzer::BlackFrame);
            if !movie && ffmpeg_valid {
                chain.push(Analyzer::Chromaprint);
            }
        }
        Mode::Introduction | Mode::Recap if !movie && ffmpeg_valid => {
            chain.push(Analyzer::Chromaprint);
        }
        _ => {}
    }
    let promote = match action {
        AnalyzerAction::Chapter => Some(Analyzer::Chapter),
        AnalyzerAction::Chromaprint => Some(Analyzer::Chromaprint),
        AnalyzerAction::BlackFrame => Some(Analyzer::BlackFrame),
        _ if prefer_chromaprint && ffmpeg_valid => Some(Analyzer::Chromaprint),
        _ => None,
    };
    if let Some(first) = promote
        && let Some(index) = chain.iter().position(|a| *a == first)
        && index > 0
    {
        let analyzer = chain.remove(index);
        chain.insert(0, analyzer);
    }
    chain
}

/// `SeasonReanalysisPlanner.IsSettledForReanalysis`.
fn is_settled(items: &[Item], c: &IntroSkipperConfig, now: DateTime<Utc>) -> bool {
    let Some(first) = items.first() else {
        return false;
    };
    if !c.reanalyze_settled_seasons
        || items.len() < SETTLED_MINIMUM_EPISODES
        || first.entry.category == Category::Movie
        || (first.entry.season_number == 0 && !c.analyze_season_zero)
    {
        return false;
    }
    // An unknown date is C#'s `default(DateTime)`: long settled.
    let newest = items.iter().filter_map(|i| i.entry.date_added).max();
    let delay = chrono::Duration::hours(i64::from(
        c.settled_season_delay_hours
            .clamp(0, MAXIMUM_SETTLED_DELAY_HOURS),
    ));
    newest.is_none_or(|newest| now - newest >= delay)
}

/// `GetSettleReanalysisModesAsync`: the modes whose action allows it and
/// whose last settled run did not cover exactly these episodes.
fn settle_modes(
    states: &HashMap<Mode, SeasonState>,
    episode_ids: &[Uuid],
    modes: &[Mode],
    ffmpeg_valid: bool,
) -> Vec<Mode> {
    modes
        .iter()
        .copied()
        .filter(|mode| {
            let state = states.get(mode);
            let action = state.map_or(AnalyzerAction::Default, |s| s.action);
            action != AnalyzerAction::None
                && can_settle_run(*mode, action, ffmpeg_valid)
                && state.is_none_or(|s| {
                    s.settled_episode_ids.len() != episode_ids.len()
                        || episode_ids
                            .iter()
                            .any(|id| !s.settled_episode_ids.contains(id))
                })
        })
        .collect()
}

/// `CanSettleReanalysisRun`: an intro can only be settled through Chromaprint
/// or chapters; the other modes always can.
fn can_settle_run(mode: Mode, action: AnalyzerAction, ffmpeg_valid: bool) -> bool {
    mode != Mode::Introduction || ffmpeg_valid || action == AnalyzerAction::Chapter
}

/// `HasUncachedAnalysisWork`: some item still needs the mode analysed.
fn has_uncached_work(items: &[Item], mode: Mode) -> bool {
    items
        .iter()
        .any(|i| i.get(mode) == EpisodeState::NotAnalyzed)
}

/// `VerifyQueueAsync`'s per-item states: a user-provided segment of a mode
/// always counts; an automatic one only under the current hash; an analysis
/// without a result (the item is in the mode's analysed list) only under the
/// current hash too. `analyze_again` reopens everything but user input.
fn derive_states(
    item: &mut Item,
    segments: &[StoredSegment],
    modes: &[Mode],
    states: &HashMap<Mode, SeasonState>,
    hash_matches: &HashMap<Mode, bool>,
    analyze_again: bool,
) {
    for &mode in modes {
        let matches = !analyze_again && hash_matches.get(&mode).copied().unwrap_or(false);
        let of_mode: Vec<&StoredSegment> = segments.iter().filter(|s| s.mode == mode).collect();
        if !of_mode.is_empty() {
            if of_mode.iter().any(|s| s.is_user_provided) {
                item.set(mode, EpisodeState::UserProvided);
            } else if matches {
                item.set(mode, EpisodeState::Analyzed);
            }
        } else if matches
            && states
                .get(&mode)
                .is_some_and(|s| s.episode_ids.contains(&item.entry.episode_id))
        {
            item.set(mode, EpisodeState::NoSegments);
        }
    }
}

/// `ExpandSettledResetModesForDerivedSegments`: an anime preview derived from
/// the credits is reset with them.
fn expand_settled(mut modes: Vec<Mode>, anime_preview_from_credits_end: bool) -> Vec<Mode> {
    if anime_preview_from_credits_end
        && modes.contains(&Mode::Credits)
        && !modes.contains(&Mode::Preview)
    {
        modes.push(Mode::Preview);
    }
    modes
}

/// `ComputeAnimePreviewFromCredits`: from the end of valid credits to the end
/// of the episode, unless such a preview is already there.
fn anime_preview(duration: f64, timestamps: &HashMap<Mode, StoredSegment>) -> Option<(f64, f64)> {
    let credits = timestamps.get(&Mode::Credits).filter(|c| c.end > 0.0)?;
    if credits.end >= duration {
        return None;
    }
    if timestamps.get(&Mode::Preview).is_some_and(|p| {
        p.end > 0.0
            && (p.start - credits.end).abs() <= ANIME_PREVIEW_TOLERANCE
            && (p.end - duration).abs() <= ANIME_PREVIEW_TOLERANCE
    }) {
        return None;
    }
    Some((credits.end, duration))
}

/// `GetMaximumSegmentDuration` (C# `int` truncation kept).
#[allow(clippy::cast_possible_truncation)]
fn maximum_duration(entry: &QueuedEpisode, mode: Mode, c: &IntroSkipperConfig) -> f64 {
    match mode {
        Mode::Introduction => f64::from(c.maximum_intro_duration),
        Mode::Recap => f64::from(c.maximum_recap_detection_duration),
        // No perfect matches, to avoid false positives from duplicates.
        Mode::Credits => f64::from((entry.duration - entry.credits_fingerprint_start - 1.0) as i32),
        _ => f64::from(entry.duration as i32),
    }
}

/// The comparison settings (`GetMinimumRegionDuration`: a recap card's 3 s,
/// otherwise `MinimumIntroDuration` — for credits too, as upstream). `None`
/// when nothing can match: upstream rejects a point whose differing bits
/// exceed `MaximumFingerprintPointDifferences`, so a negative one rejects all.
fn compare_config(c: &IntroSkipperConfig, mode: Mode) -> Option<CompareConfig> {
    Some(CompareConfig {
        inverted_index_shift: c.inverted_index_shift,
        max_bit_diff: u32::try_from(c.maximum_fingerprint_point_differences).ok()?,
        max_time_skip: c.maximum_time_skip,
        min_region_duration: if mode == Mode::Recap {
            RECAP_CARD_MINIMUM
        } else {
            f64::from(c.minimum_intro_duration)
        },
    })
}

/// `TimeAdjustmentHelper`'s nearest-candidate pick.
fn nearest(candidates: impl Iterator<Item = f64>, reference: f64) -> f64 {
    let mut best = (f64::MAX, reference);
    for value in candidates {
        let distance = (value - reference).abs();
        if distance < best.0 {
            best = (distance, value);
        }
    }
    best.1
}

/// `GetSearchRange`.
fn search_range(time: f64, duration: f64, before: f64, after: f64) -> (f64, f64) {
    ((time - before).max(0.0), (time + after).min(duration))
}

/// `GetChapterBoundary`: the chapter start nearest `reference` inside the
/// range, else `reference`.
fn chapter_boundary(chapters: &[f64], reference: f64, (start, end): (f64, f64)) -> f64 {
    nearest(
        chapters
            .iter()
            .copied()
            .filter(|t| t + EPSILON >= start && t - EPSILON <= end),
        reference,
    )
}

/// `TimeAdjustmentHelper.AdjustIntroTimesAsync`'s pure part: snap a start
/// near the episode start to 0 (no start offset then), else move it to the
/// nearest chapter and apply `IntroStartOffset`; snap an end near the episode
/// end to it, else move it to the nearest chapter and apply `IntroEndOffset`.
/// An end not snapped comes with the window silence and keyframe snapping
/// then search (see [`DetectSegmentsTask::adjust_intro_times`]).
#[derive(Debug, Clone, Copy, PartialEq)]
struct Adjusted {
    start: f64,
    end: f64,
    window: Option<(f64, f64)>,
}

fn adjust_times(
    c: &IntroSkipperConfig,
    duration: f64,
    chapters: &[f64],
    (start, end): (f64, f64),
) -> Adjusted {
    if c.end_snap_threshold < 0.0 || c.adjust_window_inward < 0.0 || c.adjust_window_outward < 0.0 {
        // Once per run of the server, not per segment (upstream logs each).
        static LOGGED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !LOGGED.swap(true, Ordering::Relaxed) {
            tracing::error!(
                "intro skipper: EndSnapThreshold, AdjustWindowInward or AdjustWindowOutward is negative; times are left unadjusted"
            );
        }
        return Adjusted {
            start,
            end,
            window: None,
        };
    }
    let adjusted_start = if start < 0.0 || start <= c.end_snap_threshold + EPSILON {
        0.0
    } else {
        let moved = if chapters.is_empty() {
            start
        } else {
            let range = search_range(
                start,
                duration,
                c.adjust_window_outward,
                c.adjust_window_inward,
            );
            chapter_boundary(chapters, start, range)
        };
        (moved + f64::from(c.intro_start_offset)).clamp(0.0, duration.max(0.0))
    };
    if end >= duration - c.end_snap_threshold - EPSILON {
        return Adjusted {
            start: adjusted_start,
            end: duration,
            window: None,
        };
    }
    let moved = if chapters.is_empty() {
        end
    } else {
        let range = search_range(
            end,
            duration,
            c.adjust_window_inward,
            c.adjust_window_outward,
        );
        chapter_boundary(chapters, end, range)
    };
    let adjusted_end = (moved - f64::from(c.intro_end_offset)).clamp(0.0, duration.max(0.0));
    Adjusted {
        start: adjusted_start,
        end: adjusted_end,
        window: Some(search_range(
            adjusted_end,
            duration,
            c.adjust_window_inward,
            c.adjust_window_outward,
        )),
    }
}

/// The adjusted times, unless the start is not before the end: the original.
fn settle_times(original: (f64, f64), start: f64, end: f64) -> (f64, f64) {
    if start >= end { original } else { (start, end) }
}

/// `AdjustIntroEndBasedOnSilenceAsync`'s pick: the start of the first
/// silence long enough that overlaps the window and starts inside it.
fn silence_end(silences: &[(f64, f64)], window: (f64, f64), minimum: f64, end: f64) -> f64 {
    silences
        .iter()
        .find(|(start, stop)| {
            window.0 < *stop && *start < window.1 && stop - start >= minimum && *start >= window.0
        })
        .map_or(end, |(start, _)| *start)
}

/// The Chromaprint mode a plugin mode fingerprints as.
fn print_mode(mode: Mode) -> Option<PrintMode> {
    match mode {
        Mode::Introduction => Some(PrintMode::Introduction),
        Mode::Credits => Some(PrintMode::Credits),
        Mode::Recap => Some(PrintMode::Recap),
        _ => None,
    }
}

impl DetectSegmentsTask {
    /// `AnalyzeItemsAsync(progress, seasonsToAnalyze)`: every non-excluded
    /// season of `queue`, `MaxParallelism` at a time; how many items were
    /// newly analysed.
    pub(super) async fn analyze_queue(
        &self,
        queue: &[(Uuid, Vec<QueuedEpisode>)],
        config: &IntroSkipperConfig,
        progress: &TaskProgress,
    ) -> usize {
        let modes = modes(config);
        let seasons: Vec<(Uuid, Vec<QueuedEpisode>)> = queue
            .iter()
            .map(|(season, entries)| {
                (
                    *season,
                    entries
                        .iter()
                        .filter(|e| !e.is_excluded)
                        .cloned()
                        .collect::<Vec<_>>(),
                )
            })
            .filter(|(_, entries)| !entries.is_empty())
            .collect();
        let total = seasons.iter().map(|(_, e)| e.len()).sum::<usize>() * modes.len();
        if total == 0 {
            tracing::info!(
                "intro skipper: no libraries selected for analysis (library options › media segment providers)"
            );
            return 0;
        }
        let count = seasons.len();
        tracing::info!(seasons = count, "intro skipper: analysis started");
        let processed = AtomicUsize::new(0);
        let parallelism = usize::try_from(config.max_parallelism.max(1)).unwrap_or(1);
        let analyzed = futures_util::stream::iter(seasons.into_iter().enumerate())
            .map(|(index, (season, entries))| {
                // Logged before the season's fingerprinting (the slow part),
                // so a stall shows as the last such line.
                tracing::info!(
                    season = index + 1,
                    of = count,
                    episodes = entries.len(),
                    "intro skipper: analyzing season"
                );
                self.analyze_season_queue(
                    season,
                    entries,
                    config,
                    &modes,
                    (progress, &processed, total),
                )
            })
            .buffer_unordered(parallelism)
            .fold(0, |sum, n| async move { sum + n })
            .await;
        // A pass consumes `AnalyzeAgain` (upstream resets it after any
        // `AnalyzeItemsAsync` that had work).
        self.runtime.analyze_again.store(false, Ordering::SeqCst);
        analyzed
    }

    /// One season of `AnalyzeItemsAsync`. The season's whole share of the
    /// progress is reported however it ends.
    async fn analyze_season_queue(
        &self,
        season_id: Uuid,
        entries: Vec<QueuedEpisode>,
        config: &IntroSkipperConfig,
        modes: &[Mode],
        (progress, processed, total): (&TaskProgress, &AtomicUsize, usize),
    ) -> usize {
        let share = entries.len() * modes.len();
        let analyzed = self
            .analyze_season_items(season_id, entries, config, modes)
            .await;
        let done = processed.fetch_add(share, Ordering::Relaxed) + share;
        #[allow(clippy::cast_precision_loss)]
        progress.report(100.0 * done as f64 / total.max(1) as f64);
        analyzed
    }

    async fn analyze_season_items(
        &self,
        season_id: Uuid,
        entries: Vec<QueuedEpisode>,
        config: &IntroSkipperConfig,
        modes: &[Mode],
    ) -> usize {
        let fingerprinter = self.fingerprinter.as_deref();
        let ffmpeg_valid = fingerprinter.is_some();
        let states = match self.actions.season_states(season_id).await {
            Ok(states) => states,
            Err(err) => {
                tracing::error!(%err, %season_id, "intro skipper: could not read the season state");
                return 0;
            }
        };
        let mut items = match self
            .verify_queue(entries, modes, &states, config, ffmpeg_valid)
            .await
        {
            Ok(items) => items,
            Err(err) => {
                tracing::error!(%err, %season_id, "intro skipper: could not read the season's segments");
                return 0;
            }
        };
        if items.is_empty() {
            return 0;
        }
        let ids: Vec<Uuid> = items.iter().map(|i| i.entry.episode_id).collect();
        let mut update = false;
        // A settled season is analysed again from scratch, so segments first
        // derived from part of it are recomputed against all of it (from the
        // cached fingerprints: only the comparison re-runs).
        let mut settled = Vec::new();
        if is_settled(&items, config, Utc::now()) {
            settled = settle_modes(&states, &ids, modes, ffmpeg_valid);
            if !settled.is_empty() {
                let reset = expand_settled(settled.clone(), config.anime_preview_from_credits_end);
                tracing::info!(%season_id, episodes = items.len(), "intro skipper: reanalysing a settled season");
                if let Err(err) = self.actions.reset_season(season_id, &ids, &reset).await {
                    tracing::error!(%err, %season_id, "intro skipper: could not reset the settled season");
                    return 0;
                }
                for item in &mut items {
                    for mode in &reset {
                        if item.get(*mode) != EpisodeState::UserProvided {
                            item.set(*mode, EpisodeState::NotAnalyzed);
                        }
                    }
                }
                // Publish even when the recompute finds nothing.
                update = true;
            }
        }
        let mut analyzed_total = 0;
        let mut completed = Vec::new();
        for &mode in modes {
            match self
                .analyze_mode(season_id, &mut items, mode, &states, config, fingerprinter)
                .await
            {
                Ok(analyzed) => {
                    analyzed_total += analyzed;
                    update |= analyzed > 0;
                    if settled.contains(&mode) {
                        completed.push(mode);
                    }
                }
                // Upstream's exception ends the season's pass: nothing more is
                // recorded, so the season is retried on the next one.
                Err(err) => {
                    tracing::error!(%err, %season_id, ?mode, "intro skipper: analysis failed; the season is retried next pass");
                    break;
                }
            }
        }
        if update && config.update_media_segments {
            for &item_id in &ids {
                if let Err(err) = intro_store::refresh(
                    self.actions.as_ref(),
                    self.media_segments.as_ref(),
                    item_id,
                )
                .await
                {
                    tracing::error!(%err, %item_id, "intro skipper: publishing media segments failed");
                }
            }
        }
        if !completed.is_empty()
            && let Err(err) = self
                .actions
                .record_settled(season_id, &completed, &ids)
                .await
        {
            tracing::warn!(%err, %season_id, "intro skipper: could not record the settled reanalysis");
        }
        analyzed_total
    }

    /// `VerifyQueueAsync`: items whose file is gone are dropped, and each
    /// item's per-mode state is read back from one snapshot of the season.
    async fn verify_queue(
        &self,
        entries: Vec<QueuedEpisode>,
        modes: &[Mode],
        states: &HashMap<Mode, SeasonState>,
        config: &IntroSkipperConfig,
        ffmpeg_valid: bool,
    ) -> Result<Vec<Item>, ServiceError> {
        // `AnalyzeAgain`: the settings changed since the last pass.
        let analyze_again = self.runtime.analyze_again.load(Ordering::SeqCst);
        let hash_matches: HashMap<Mode, bool> = modes
            .iter()
            .map(|&mode| {
                let action = states
                    .get(&mode)
                    .map_or(AnalyzerAction::Default, |s| s.action);
                let expected = config_hash::analysis(config, mode, action, ffmpeg_valid);
                (
                    mode,
                    states.get(&mode).is_some_and(|s| s.config_hash == expected),
                )
            })
            .collect();
        let ids: Vec<Uuid> = entries.iter().map(|e| e.episode_id).collect();
        let segments = self.actions.segments_of(&ids).await?;
        let mut verified = Vec::with_capacity(entries.len());
        for entry in entries {
            if !tokio::fs::try_exists(&entry.path).await.unwrap_or(false) {
                tracing::debug!(
                    item = entry.name,
                    "intro skipper: skipping, the file is gone"
                );
                continue;
            }
            let of_item = segments
                .get(&entry.episode_id)
                .map_or(&[][..], Vec::as_slice);
            let mut item = Item::new(entry);
            derive_states(
                &mut item,
                of_item,
                modes,
                states,
                &hash_matches,
                analyze_again,
            );
            verified.push(item);
        }
        Ok(verified)
    }

    /// One mode of a season (`AnalyzeItemsAsync(plugin, items, mode)`): how
    /// many items it newly analysed. A store failure is an error, so the
    /// mode's analysed list is not recorded and the items are retried.
    async fn analyze_mode(
        &self,
        season_id: Uuid,
        items: &mut [Item],
        mode: Mode,
        states: &HashMap<Mode, SeasonState>,
        config: &IntroSkipperConfig,
        fingerprinter: Option<&dyn Fingerprinter>,
    ) -> Result<usize, ServiceError> {
        if !has_uncached_work(items, mode) {
            return Ok(0);
        }
        let first = &items[0].entry;
        let category = first.category;
        if category != Category::Movie && first.season_number == 0 && !config.analyze_season_zero {
            return Ok(0);
        }
        let total = items
            .iter()
            .filter(|i| i.get(mode) != EpisodeState::Analyzed)
            .count();
        let action_of = |m: Mode| states.get(&m).map_or(AnalyzerAction::Default, |s| s.action);
        let action = action_of(mode);
        let ffmpeg_valid = fingerprinter.is_some();
        let hash = config_hash::analysis(config, mode, action, ffmpeg_valid);
        if action == AnalyzerAction::None {
            tracing::debug!(?mode, %season_id, "intro skipper: mode skipped by the season's analyzer action");
            return Ok(0);
        }
        tracing::debug!(?mode, %season_id, items = items.len(), "intro skipper: analyzing files");
        for item in items.iter_mut() {
            hash.clone_into(&mut item.config_hash);
        }
        let automatic: Vec<Uuid> = items
            .iter()
            .filter(|i| i.get(mode) != EpisodeState::UserProvided)
            .map(|i| i.entry.episode_id)
            .collect();
        self.actions
            .clean_stale_automatic(&automatic, mode, &hash)
            .await?;
        for analyzer in analyzer_chain(
            mode,
            category,
            ffmpeg_valid,
            action,
            config.prefer_chromaprint,
        ) {
            match analyzer {
                Analyzer::Chromaprint => {
                    if let Some(fingerprinter) = fingerprinter {
                        self.chromaprint(items, mode, config, fingerprinter).await?;
                    }
                }
                Analyzer::Chapter => self.chapter_analyzer(items, mode, config).await?,
                Analyzer::BlackFrame if config.use_alternative_black_frame_analyzer => {
                    self.credit_scene_analyzer(items, config).await?;
                }
                Analyzer::BlackFrame => self.black_frame_analyzer(items, config).await?,
            }
        }
        if mode == Mode::Credits
            && category == Category::AnimeEpisode
            && config.anime_preview_from_credits_end
        {
            // Stored under the Preview mode's hash, not the Credits one
            // upstream uses: Preview's own stale-segment cleanup (it runs
            // next, `ScanPreview` being on by default) deletes a preview
            // under any other hash, after which the analysed credits never
            // recreate it — an upstream bug not ported.
            let preview_hash = config_hash::analysis(
                config,
                Mode::Preview,
                action_of(Mode::Preview),
                ffmpeg_valid,
            );
            self.anime_previews(items, &preview_hash).await?;
        }
        let ids: Vec<Uuid> = items.iter().map(|i| i.entry.episode_id).collect();
        self.actions
            .set_episode_ids(season_id, mode, &ids, &hash)
            .await?;
        Ok(total
            - items
                .iter()
                .filter(|i| i.get(mode) != EpisodeState::Analyzed)
                .count())
    }

    /// `ChromaprintAnalyzer.AnalyzeMediaFiles`: fingerprint the items still
    /// needing analysis (and analysed ones whose prints are cached), then walk
    /// them as a queue — each compared with the rest until its first valid
    /// match — keeping each episode's longest shared region, adjusted and
    /// stored as it leaves the queue.
    async fn chromaprint(
        &self,
        items: &mut [Item],
        mode: Mode,
        config: &IntroSkipperConfig,
        fingerprinter: &dyn Fingerprinter,
    ) -> Result<(), ServiceError> {
        let Some(print) = print_mode(mode) else {
            return Ok(());
        };
        let mut queue: Vec<usize> = Vec::new();
        for (index, item) in items.iter().enumerate() {
            if item.needs_analysis(mode)
                || (item.get(mode) == EpisodeState::Analyzed
                    && self.has_cached_print(item, mode, config).await)
            {
                queue.push(index);
            }
        }
        if items.len() <= 1
            || queue
                .iter()
                .all(|&i| items[i].get(mode) == EpisodeState::Analyzed)
        {
            return Ok(());
        }
        let Some(cmp) = compare_config(config, mode) else {
            return Ok(());
        };
        // At least two prints: a lone episode is compared with its neighbours.
        if let [lone] = queue[..] {
            let number = items[lone].entry.episode_number;
            queue.extend(
                (0..items.len())
                    .filter(|&i| i != lone && (items[i].entry.episode_number - number).abs() <= 1),
            );
        }
        let prints = self
            .prints(items, &queue, mode, print, fingerprinter, config)
            .await;
        let mut found: HashMap<usize, TimeRange> = HashMap::new();
        let mut recap_frames = HashMap::new();
        while !queue.is_empty() {
            let current = queue.remove(0);
            for &remaining in &queue {
                // CPU-bound: off the async workers, so parallel seasons
                // compare in parallel.
                let (lhs, rhs) = (
                    Arc::clone(&prints[&current]),
                    Arc::clone(&prints[&remaining]),
                );
                let (lhs, rhs) =
                    tokio::task::spawn_blocking(move || compare_episodes(&lhs, &rhs, print, &cmp))
                        .await
                        .map_err(|e| ServiceError::backend(e.to_string()))?;
                let (Some(mut lhs), Some(mut rhs)) = (lhs, rhs) else {
                    continue;
                };
                let maximum = maximum_duration(&items[remaining].entry, mode, config);
                if rhs.end <= 0.0 || rhs.duration() > maximum {
                    continue;
                }
                // A shared recap card is the recap's start; its end is the
                // last black frame after the card.
                if mode == Mode::Recap {
                    let pair = self
                        .recap_pair(
                            (&items[current].entry, &items[remaining].entry),
                            ((lhs.start, lhs.end), (rhs.start, rhs.end)),
                            (maximum, config),
                            &mut recap_frames,
                        )
                        .await?;
                    let Some((l, r)) = pair else {
                        continue;
                    };
                    (lhs, rhs) = (TimeRange::new(l.0, l.1), TimeRange::new(r.0, r.1));
                }
                // The prints start at the credits window, not at 0.
                if mode == Mode::Credits {
                    let (l, r) = (
                        items[current].entry.credits_fingerprint_start,
                        items[remaining].entry.credits_fingerprint_start,
                    );
                    lhs = TimeRange::new(lhs.start + l, lhs.end + l);
                    rhs = TimeRange::new(rhs.start + r, rhs.end + r);
                }
                for (index, range) in [(current, lhs), (remaining, rhs)] {
                    if found
                        .get(&index)
                        .is_none_or(|kept| range.duration() > kept.duration())
                    {
                        found.insert(index, range);
                    }
                }
                break;
            }
            if let Some(range) = found.get(&current).copied() {
                let snap_to_chapters = config.adjust_intro_based_on_chapters;
                self.store_found(
                    &mut items[current],
                    mode,
                    (range.start, range.end),
                    config,
                    snap_to_chapters,
                )
                .await?;
            }
        }
        Ok(())
    }

    /// The queued items' prints for `mode` (cached on disk); a failed one is
    /// empty, as upstream falls back to `[]`.
    async fn prints(
        &self,
        items: &[Item],
        queue: &[usize],
        mode: Mode,
        print: PrintMode,
        fingerprinter: &dyn Fingerprinter,
        config: &IntroSkipperConfig,
    ) -> HashMap<usize, Arc<Vec<u32>>> {
        let mut prints = HashMap::new();
        for &index in queue {
            let item = &items[index];
            let fingerprint = match item.fingerprint_range(mode) {
                Some((start, end)) => self
                    .fingerprint_cached(&item.entry, (start, end), print, fingerprinter, config)
                    .await
                    .unwrap_or_else(|err| {
                        tracing::debug!(%err, path = item.entry.path, ?mode, "intro skipper: fingerprint failed — no segment for this window");
                        Vec::new()
                    }),
                None => Vec::new(),
            };
            prints.insert(index, Arc::new(fingerprint));
        }
        prints
    }

    /// `CreateAnimePreviewFromCreditsAsync` (under the Preview hash, see the
    /// caller).
    async fn anime_previews(
        &self,
        items: &mut [Item],
        preview_hash: &str,
    ) -> Result<(), ServiceError> {
        for item in items.iter_mut() {
            let id = item.entry.episode_id;
            let segments = self.actions.segments(id).await?;
            if segments
                .iter()
                .any(|s| s.mode == Mode::Preview && s.is_user_provided)
            {
                continue;
            }
            let Some((start, end)) =
                anime_preview(item.entry.duration, &intro_store::timestamps(&segments))
            else {
                continue;
            };
            self.actions
                .update_timestamp(StoredSegment {
                    item_id: id,
                    mode: Mode::Preview,
                    start,
                    end,
                    is_user_provided: false,
                    config_hash: preview_hash.to_owned(),
                })
                .await?;
            item.set(Mode::Preview, EpisodeState::Analyzed);
        }
        Ok(())
    }

    /// An analyzer's find for the item: adjusted (`AdjustIntroTimesAsync`),
    /// stored under the mode's hash, and the item marked analysed.
    pub(super) async fn store_found(
        &self,
        item: &mut Item,
        mode: Mode,
        found: (f64, f64),
        config: &IntroSkipperConfig,
        snap_to_chapters: bool,
    ) -> Result<(), ServiceError> {
        let adjusted = self
            .adjust_intro_times(&item.entry, mode, found, config, snap_to_chapters)
            .await;
        self.store_segment(item, mode, adjusted).await
    }

    /// A find stored as it is, under the mode's hash, and the item marked
    /// analysed.
    pub(super) async fn store_segment(
        &self,
        item: &mut Item,
        mode: Mode,
        (start, end): (f64, f64),
    ) -> Result<(), ServiceError> {
        self.actions
            .update_timestamp(StoredSegment {
                item_id: item.entry.episode_id,
                mode,
                start,
                end,
                is_user_provided: false,
                config_hash: item.config_hash.clone(),
            })
            .await?;
        item.set(mode, EpisodeState::Analyzed);
        Ok(())
    }

    /// `AdjustIntroTimesAsync`: the pure adjustment, then an end not snapped
    /// to the episode end moved to the next silence (`AdjustIntroBasedOnSilence`)
    /// and to the nearest keyframe (`SnapToKeyframe`) within the window. A
    /// failed detection leaves the end where it is — for keyframes a
    /// deliberate divergence: upstream's spawn failure there escapes the
    /// analyzer and fails the season, over a refinement.
    async fn adjust_intro_times(
        &self,
        entry: &QueuedEpisode,
        mode: Mode,
        original: (f64, f64),
        config: &IntroSkipperConfig,
        use_chapters: bool,
    ) -> (f64, f64) {
        let chapters = if use_chapters {
            self.chapter_starts(entry.episode_id).await
        } else {
            Vec::new()
        };
        let adjusted = adjust_times(config, entry.duration, &chapters, original);
        let Some(window) = adjusted.window else {
            return settle_times(original, adjusted.start, adjusted.end);
        };
        let options = ProcessOptions::new(&config.process_priority, config.process_threads);
        let mut end = adjusted.end;
        if config.adjust_intro_based_on_silence {
            match self.silences(entry, mode, window, config, options).await {
                Ok(silences) => {
                    end = silence_end(
                        &silences,
                        window,
                        config.silence_detection_minimum_duration,
                        end,
                    );
                }
                Err(err) => {
                    tracing::debug!(%err, item = entry.name, "intro skipper: silence detection failed");
                }
            }
        }
        if config.snap_to_keyframe {
            match self.keyframes(entry, mode, window, config, options).await {
                Ok(keyframes) => end = nearest(keyframes.into_iter(), end),
                Err(err) => {
                    tracing::debug!(%err, item = entry.name, "intro skipper: keyframe detection failed");
                }
            }
        }
        settle_times(original, adjusted.start, end)
    }

    /// The item's chapter starts (seconds).
    async fn chapter_starts(&self, item_id: Uuid) -> Vec<f64> {
        self.item_chapters(item_id)
            .await
            .iter()
            .map(|c| chapter_analyzer::seconds(c.start_position_ticks))
            .collect()
    }

    /// The item's chapters; none when they cannot be read.
    pub(super) async fn item_chapters(&self, item_id: Uuid) -> Vec<ChapterInfo> {
        self.chapters
            .get_chapters(item_id)
            .await
            .inspect_err(
                |err| tracing::debug!(%err, %item_id, "intro skipper: chapters unreadable"),
            )
            .unwrap_or_default()
    }

    /// `HasCachedFingerprint`: with `CacheFingerprints`, the window's print is
    /// on disk.
    async fn has_cached_print(&self, item: &Item, mode: Mode, config: &IntroSkipperConfig) -> bool {
        let (Some((start, end)), Some(print)) = (item.fingerprint_range(mode), print_mode(mode))
        else {
            return false;
        };
        if !config.cache_fingerprints {
            return false;
        }
        tokio::fs::try_exists(self.print_path(item.entry.episode_id, start, end, print))
            .await
            .unwrap_or(false)
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact arithmetic
mod tests {
    use super::*;

    fn cfg() -> IntroSkipperConfig {
        IntroSkipperConfig::default()
    }

    /// The pure adjustment settled (no silence or keyframe detection).
    fn adjusted(
        c: &IntroSkipperConfig,
        duration: f64,
        chapters: &[f64],
        original: (f64, f64),
    ) -> (f64, f64) {
        let a = adjust_times(c, duration, chapters, original);
        settle_times(original, a.start, a.end)
    }

    fn entry(episode_number: i64) -> QueuedEpisode {
        QueuedEpisode {
            series_name: String::new(),
            season_number: 1,
            series_id: Uuid::nil(),
            season_id: Uuid::nil(),
            episode_number,
            episode_id: Uuid::from_u128(episode_number.unsigned_abs().into()),
            name: String::new(),
            category: Category::Episode,
            is_excluded: false,
            path: String::new(),
            duration: 1800.7,
            date_added: None,
            intro_fingerprint_end: 450.0,
            credits_fingerprint_start: 1350.0,
            credits_fingerprint_end: 1800.0,
        }
    }

    fn seg(mode: Mode, start: f64, end: f64) -> StoredSegment {
        StoredSegment {
            item_id: Uuid::nil(),
            mode,
            start,
            end,
            is_user_provided: false,
            config_hash: String::new(),
        }
    }

    #[test]
    fn the_chain_follows_analyze_items_async() {
        use Analyzer::{BlackFrame as B, Chapter as C, Chromaprint as P};
        use Category::{AnimeEpisode, Episode, Movie};
        let d = AnalyzerAction::Default;
        assert_eq!(
            analyzer_chain(Mode::Introduction, Episode, true, d, false),
            [C, P]
        );
        assert_eq!(
            analyzer_chain(Mode::Introduction, Movie, true, d, false),
            [C]
        );
        assert_eq!(
            analyzer_chain(Mode::Credits, Episode, true, d, false),
            [C, B, P]
        );
        assert_eq!(
            analyzer_chain(Mode::Credits, AnimeEpisode, true, d, false),
            [C, P, B]
        );
        assert_eq!(analyzer_chain(Mode::Credits, Movie, true, d, false), [C, B]);
        assert_eq!(analyzer_chain(Mode::Preview, Episode, true, d, false), [C]);
        // Promotion: the action wins over PreferChromaprint.
        assert_eq!(
            analyzer_chain(Mode::Credits, Episode, true, d, true),
            [P, C, B]
        );
        assert_eq!(
            analyzer_chain(
                Mode::Credits,
                Episode,
                true,
                AnalyzerAction::BlackFrame,
                true
            ),
            [B, C, P]
        );
        assert_eq!(
            analyzer_chain(Mode::Introduction, Episode, false, d, true),
            [C]
        );
    }

    #[test]
    fn time_adjustment_follows_the_helper() {
        let c = IntroSkipperConfig {
            intro_start_offset: 2,
            intro_end_offset: 3,
            ..cfg()
        };
        // A start within EndSnapThreshold (2 s) snaps to 0 and skips the offset.
        assert_eq!(adjusted(&c, 1800.0, &[], (1.5, 60.0)), (0.0, 57.0));
        // Otherwise the offsets apply; an end near the episode end snaps to it.
        assert_eq!(adjusted(&c, 1800.0, &[], (30.0, 1799.0)), (32.0, 1800.0));
        // Chapters inside the window win: start window is [t-2, t+5].
        assert_eq!(
            adjusted(&cfg(), 1800.0, &[34.0, 120.0], (30.0, 118.0)),
            (34.0, 120.0)
        );
        // A start not before the end keeps the original.
        assert_eq!(adjusted(&c, 1800.0, &[], (30.0, 31.0)), (30.0, 31.0));
    }

    #[test]
    fn durations_follow_the_analyzer() {
        let entry = entry(1);
        assert_eq!(maximum_duration(&entry, Mode::Credits, &cfg()), 449.0);
        assert_eq!(maximum_duration(&entry, Mode::Introduction, &cfg()), 120.0);
        // Credits take MinimumIntroDuration as their minimum match, as upstream.
        let c = IntroSkipperConfig {
            minimum_intro_duration: 15,
            minimum_credits_duration: 40,
            ..cfg()
        };
        let min = |mode| {
            compare_config(&c, mode)
                .expect("comparable")
                .min_region_duration
        };
        assert_eq!(min(Mode::Credits), 15.0);
        assert_eq!(min(Mode::Recap), 3.0);
        // A negative MaximumFingerprintPointDifferences rejects every point.
        let none = IntroSkipperConfig {
            maximum_fingerprint_point_differences: -1,
            ..cfg()
        };
        assert!(compare_config(&none, Mode::Introduction).is_none());
    }

    #[test]
    fn settled_seasons_and_anime_previews() {
        let state = SeasonState {
            settled_episode_ids: [Uuid::from_u128(1), Uuid::from_u128(2)].into(),
            ..SeasonState::default()
        };
        let states = HashMap::from([(Mode::Credits, state)]);
        let ids = [Uuid::from_u128(1), Uuid::from_u128(2)];
        let modes = [Mode::Introduction, Mode::Credits];
        // Credits already covered these episodes; the intro has no state.
        assert_eq!(
            settle_modes(&states, &ids, &modes, true),
            [Mode::Introduction]
        );
        // Without Chromaprint an intro can only be settled through chapters.
        assert!(settle_modes(&states, &ids, &modes, false).is_empty());
        assert_eq!(
            expand_settled(vec![Mode::Credits], true),
            [Mode::Credits, Mode::Preview]
        );
        let credits = HashMap::from([(Mode::Credits, seg(Mode::Credits, 1300.0, 1400.0))]);
        assert_eq!(anime_preview(1440.0, &credits), Some((1400.0, 1440.0)));
        let mut both = credits.clone();
        both.insert(Mode::Preview, seg(Mode::Preview, 1400.2, 1440.0));
        assert_eq!(anime_preview(1440.0, &both), None);
        assert_eq!(anime_preview(1400.0, &credits), None);
    }

    /// `TestTimeAdjustmentHelper` (intro-skipper `db09359`), its configuration:
    /// a 2 s snap threshold and window, no chapter/silence/keyframe snapping.
    #[rstest::rstest]
    // StartOffset_IsIgnored_When_SnappingToEpisodeStart
    #[case(2, 0, 60.0, (1.2, 10.0), (0.0, 10.0))]
    // StartOffset_IsApplied_When_NotSnapping
    #[case(2, 0, 60.0, (5.0, 12.0), (7.0, 12.0))]
    // Start_And_End_Are_Clamped_To_Duration
    #[case(0, 100, 30.0, (-5.0, 200.0), (0.0, 30.0))]
    fn time_adjustment_helper_cases(
        #[case] start_offset: i32,
        #[case] end_offset: i32,
        #[case] duration: f64,
        #[case] original: (f64, f64),
        #[case] expected: (f64, f64),
    ) {
        let c = IntroSkipperConfig {
            end_snap_threshold: 2.0,
            adjust_intro_based_on_chapters: false,
            adjust_intro_based_on_silence: false,
            snap_to_keyframe: false,
            adjust_window_inward: 2.0,
            adjust_window_outward: 2.0,
            intro_start_offset: start_offset,
            intro_end_offset: end_offset,
            ..cfg()
        };
        assert_eq!(adjusted(&c, duration, &[], original), expected);
    }

    /// `TestAnimePreviewRefresh`: credits and an existing preview (`(0, 0)` is
    /// an invalid, default segment) on a 22-minute episode.
    #[rstest::rstest]
    #[case::new_preview(Some((1200.0, 1260.0)), None, 1320.0, Some((1260.0, 1320.0)))]
    #[case::credits_end_shifts(Some((1200.0, 1260.0)), Some((1080.0, 1320.0)), 1320.0, Some((1260.0, 1320.0)))]
    #[case::idempotent(Some((1200.0, 1260.0)), Some((1260.0, 1320.0)), 1320.0, None)]
    #[case::sub_second_drift(Some((1200.0, 1260.3)), Some((1260.0, 1320.0)), 1320.0, None)]
    #[case::exact_boundary_drift(Some((1200.0, 1260.5)), Some((1260.0, 1320.0)), 1320.0, None)]
    #[case::duration_changes(Some((1200.0, 1260.0)), Some((1260.0, 1320.0)), 1380.0, Some((1260.0, 1380.0)))]
    #[case::drift_beyond_tolerance(Some((1200.0, 1260.0)), Some((1259.0, 1320.0)), 1320.0, Some((1260.0, 1320.0)))]
    #[case::no_credits(None, None, 1320.0, None)]
    #[case::invalid_credits(Some((0.0, 0.0)), None, 1320.0, None)]
    #[case::credits_reach_the_end(Some((1200.0, 1320.0)), None, 1320.0, None)]
    #[case::invalid_existing_preview(Some((1200.0, 1260.0)), Some((0.0, 0.0)), 1320.0, Some((1260.0, 1320.0)))]
    fn anime_preview_refresh_cases(
        #[case] credits: Option<(f64, f64)>,
        #[case] preview: Option<(f64, f64)>,
        #[case] duration: f64,
        #[case] expected: Option<(f64, f64)>,
    ) {
        let mut timestamps = HashMap::new();
        if let Some((start, end)) = credits {
            timestamps.insert(Mode::Credits, seg(Mode::Credits, start, end));
        }
        if let Some((start, end)) = preview {
            timestamps.insert(Mode::Preview, seg(Mode::Preview, start, end));
        }
        assert_eq!(anime_preview(duration, &timestamps), expected);
    }

    /// `TestSeasonReanalysis.Season`: `count` episodes, the newest added at
    /// `newest`, each earlier one an hour before.
    fn season(
        count: usize,
        newest: DateTime<Utc>,
        category: Category,
        season_number: i64,
    ) -> Vec<Item> {
        (0..count)
            .map(|i| {
                let hours = i64::try_from(i).expect("small");
                Item::new(QueuedEpisode {
                    category,
                    season_number,
                    // The queue already filtered exclusions, so the flag is moot.
                    is_excluded: true,
                    date_added: Some(newest - chrono::Duration::hours(hours)),
                    ..entry(hours + 1)
                })
            })
            .collect()
    }

    /// `TestSeasonReanalysis.IsSettledForReanalysis_*`: hours since the newest
    /// episode, the episode count, the category and season, and
    /// (`ReanalyzeSettledSeasons`/`SettledSeasonDelayHours`/`AnalyzeSeasonZero`).
    #[rstest::rstest]
    #[case::disabled(25, 3, Category::Episode, 1, (false, 24, false), false)]
    #[case::settled_multi_episode(25, 3, Category::Episode, 1, (true, 24, false), true)]
    #[case::still_receiving(1, 3, Category::Episode, 1, (true, 24, false), false)]
    #[case::default_delay_minus_one(23, 3, Category::Episode, 1, (true, 24, false), false)]
    #[case::default_delay(24, 3, Category::Episode, 1, (true, 24, false), true)]
    #[case::configured_delay_minus_one(47, 3, Category::Episode, 1, (true, 48, false), false)]
    #[case::configured_delay(48, 3, Category::Episode, 1, (true, 48, false), true)]
    #[case::zero_delay(0, 3, Category::Episode, 1, (true, 0, false), true)]
    #[case::below_minimum_episodes(25, 2, Category::Episode, 1, (true, 24, false), false)]
    #[case::movies(25, 3, Category::Movie, 1, (true, 24, false), false)]
    #[case::season_zero_disabled(25, 3, Category::Episode, 0, (true, 24, false), false)]
    #[case::season_zero_enabled(25, 3, Category::Episode, 0, (true, 24, true), true)]
    fn settled_for_reanalysis_cases(
        #[case] hours_ago: i64,
        #[case] count: usize,
        #[case] category: Category,
        #[case] season_number: i64,
        #[case] (reanalyze_settled_seasons, settled_season_delay_hours, analyze_season_zero): (
            bool,
            i32,
            bool,
        ),
        #[case] expected: bool,
    ) {
        let now = Utc::now();
        let c = IntroSkipperConfig {
            reanalyze_settled_seasons,
            settled_season_delay_hours,
            analyze_season_zero,
            ..cfg()
        };
        let items = season(
            count,
            now - chrono::Duration::hours(hours_ago),
            category,
            season_number,
        );
        assert_eq!(is_settled(&items, &c, now), expected);
    }

    /// `CanSettleReanalysisRun_SkipsIntroduction_WhenChromaprintUnavailable`.
    #[rstest::rstest]
    #[case(Mode::Introduction, AnalyzerAction::Default, false, false)]
    #[case(Mode::Introduction, AnalyzerAction::Chromaprint, false, false)]
    #[case(Mode::Introduction, AnalyzerAction::Chapter, false, true)]
    #[case(Mode::Introduction, AnalyzerAction::Default, true, true)]
    #[case(Mode::Credits, AnalyzerAction::Default, false, true)]
    fn can_settle_run_cases(
        #[case] mode: Mode,
        #[case] action: AnalyzerAction,
        #[case] ffmpeg_valid: bool,
        #[case] expected: bool,
    ) {
        assert_eq!(can_settle_run(mode, action, ffmpeg_valid), expected);
    }

    #[test]
    fn expand_settled_reset_modes_for_derived_segments() {
        assert_eq!(expand_settled(vec![Mode::Credits], false), [Mode::Credits]);
        assert_eq!(
            expand_settled(vec![Mode::Credits, Mode::Preview], true),
            [Mode::Credits, Mode::Preview]
        );
    }

    /// `HasUncachedAnalysisWork_*`.
    #[rstest::rstest]
    #[case::not_analyzed(&[EpisodeState::NotAnalyzed], true)]
    #[case::settled_no_segments(&[EpisodeState::NoSegments], false)]
    #[case::any_not_analyzed(&[EpisodeState::NoSegments, EpisodeState::NotAnalyzed], true)]
    #[case::all_handled(&[EpisodeState::Analyzed, EpisodeState::UserProvided], false)]
    fn has_uncached_analysis_work_cases(#[case] states: &[EpisodeState], #[case] expected: bool) {
        let items: Vec<Item> = states
            .iter()
            .map(|&state| {
                let mut item = Item::new(entry(1));
                item.set(Mode::Introduction, state);
                item
            })
            .collect();
        assert_eq!(has_uncached_work(&items, Mode::Introduction), expected);
    }

    /// `VerifyQueueAsync`'s states, including
    /// `VerifyQueueAsync_ReopensNoSegments_WhenChromaprintBecomesAvailable`
    /// (a hash mismatch reopens what the old hash recorded).
    #[test]
    fn verify_queue_derives_states_from_the_snapshot() {
        let item = entry(1);
        let id = item.episode_id;
        let states = HashMap::from([(
            Mode::Introduction,
            SeasonState {
                episode_ids: [id].into(),
                ..SeasonState::default()
            },
        )]);
        let modes = [Mode::Introduction];
        let derive = |segments: &[StoredSegment], matches: bool, again: bool| {
            let mut derived = Item::new(item.clone());
            let hash_matches = HashMap::from([(Mode::Introduction, matches)]);
            derive_states(
                &mut derived,
                segments,
                &modes,
                &states,
                &hash_matches,
                again,
            );
            derived.get(Mode::Introduction)
        };
        let automatic = [seg(Mode::Introduction, 0.0, 30.0)];
        let user = [StoredSegment {
            is_user_provided: true,
            ..seg(Mode::Introduction, 0.0, 30.0)
        }];
        assert_eq!(derive(&[], true, false), EpisodeState::NoSegments);
        assert_eq!(derive(&[], false, false), EpisodeState::NotAnalyzed);
        assert_eq!(derive(&automatic, true, false), EpisodeState::Analyzed);
        assert_eq!(derive(&automatic, false, false), EpisodeState::NotAnalyzed);
        assert_eq!(derive(&automatic, true, true), EpisodeState::NotAnalyzed);
        assert_eq!(derive(&user, false, true), EpisodeState::UserProvided);
        // Not in the mode's analysed list: never analysed.
        let mut other = Item::new(entry(2));
        let hash_matches = HashMap::from([(Mode::Introduction, true)]);
        derive_states(&mut other, &[], &modes, &states, &hash_matches, false);
        assert_eq!(other.get(Mode::Introduction), EpisodeState::NotAnalyzed);
    }

    /// `AdjustIntroEndBasedOnSilenceAsync`: the first silence that overlaps
    /// the window, starts inside it and lasts `SilenceDetectionMinimumDuration`.
    #[rstest::rstest]
    #[case::starts_before_the_window(&[(40.0, 45.0)], 49.5)]
    #[case::too_short(&[(46.0, 46.1)], 49.5)]
    #[case::outside(&[(52.0, 53.0)], 49.5)]
    #[case::first_fit(&[(40.0, 45.0), (46.0, 47.0), (48.0, 49.0)], 46.0)]
    fn silence_end_cases(#[case] silences: &[(f64, f64)], #[case] expected: f64) {
        assert_eq!(silence_end(silences, (44.5, 51.5), 0.33, 49.5), expected);
    }

    /// Only an end not snapped to the episode end is refined, over
    /// `GetSearchRange(end, duration, AdjustWindowInward, AdjustWindowOutward)`.
    #[test]
    fn the_refinement_window_follows_the_adjusted_end() {
        let c = IntroSkipperConfig {
            adjust_window_inward: 5.0,
            adjust_window_outward: 2.0,
            intro_end_offset: 1,
            ..cfg()
        };
        let a = adjust_times(&c, 1800.0, &[], (10.0, 50.0));
        assert_eq!((a.end, a.window), (49.0, Some((44.0, 51.0))));
        assert_eq!(adjust_times(&c, 1800.0, &[], (10.0, 1799.0)).window, None);
    }
}
