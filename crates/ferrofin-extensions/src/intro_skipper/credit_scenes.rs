//! The alternative credits analyzer — port of the plugin's
//! `CreditsBlackFrameAnalyzer` and its `Analyzers/Credits` helpers
//! (`CreditSceneBuilder`, `CreditSceneMetrics`, `CreditsBoundaryHelper`,
//! `CreditsBoundaryRefiner`, `CreditEntropyFallback`, `CreditDetectionPolicy`)
//! (intro-skipper `db09359`), selected by `UseAlternativeBlackFrameAnalyzer`.
//!
//! One keyframe scan of the credits window gives each keyframe's black
//! percentage; dense runs of black keyframes are credit scenes, confirmed by
//! `blackdetect` intervals when sparse, ranked, and the chosen one's start
//! refined between keyframes. Without one, credits on a muted, uniform card
//! are found from the keyframes' entropy and saturation
//! (`DetectNonBlackCredits`). Times are relative to the credits window's
//! start until a segment is made.

use ferrofin_model::intro_skipper::AnalysisMode as Mode;
use ferrofin_traits::error::ServiceError;

use super::engine::Item;
use super::queue::QueuedEpisode;
use super::{DetectSegmentsTask, IntroSkipperConfig};
use crate::ffmpeg::{BlackFrame, BlackInterval};

/// `CreditDetectionPolicy`.
mod policy {
    /// Black runs this close merge; also the entropy fallback's run bridge.
    pub const MAXIMUM_SCENE_MERGE_GAP: f64 = 20.0;
    /// A run breaks at a gap this many median keyframe gaps long.
    pub const MAXIMUM_KEYFRAME_GAP_MULTIPLIER: f64 = 5.0;
    /// The share of a scene's keyframes that must be black.
    pub const MINIMUM_BLACK_FRAME_DENSITY: f64 = 0.50;
    /// How far before a scene's first black keyframe an interval may end and
    /// still support it.
    pub const MAXIMUM_INTERVAL_TO_KEYFRAME_GAP: f64 = 2.0;
    /// The overlap with a `blackdetect` interval that counts as support.
    pub const MINIMUM_INTERVAL_OVERLAP: f64 = 0.25;
    /// The keyframe gap before a scene worth probing for its real start.
    pub const MINIMUM_BOUNDARY_PROBE_WINDOW: f64 = 0.50;

    /// A scene is sparse when its black keyframes are on average further
    /// apart than half the minimum credits duration.
    pub fn maximum_sparse_average_black_frame_gap(minimum_duration: i32) -> f64 {
        f64::from(minimum_duration) * 0.5
    }

    /// The `blackdetect` probe reaches a minimum credits duration past a
    /// candidate.
    pub fn interval_probe_padding(minimum_duration: i32) -> f64 {
        f64::from(minimum_duration)
    }
}

/// A run of black keyframes (`CreditScene`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct CreditScene {
    pub start_frame: i64,
    pub end_frame: i64,
    pub start_time: f64,
    pub end_time: f64,
}

impl CreditScene {
    fn new(start_frame: i64, end_frame: i64, start_time: f64, end_time: f64) -> Self {
        Self {
            start_frame,
            end_frame,
            start_time,
            end_time,
        }
    }

    fn duration(&self) -> f64 {
        self.end_time - self.start_time
    }
}

/// `CreditSceneMetrics`: a scene's keyframes and black keyframes.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Metrics {
    total: usize,
    black: usize,
}

impl Metrics {
    /// `CreditSceneMetricsCalculator.Calculate` (frames are in time order).
    fn of(frames: &[BlackFrame], scene: &CreditScene, minimum: i32) -> Self {
        let mut metrics = Self { total: 0, black: 0 };
        for frame in frames {
            if frame.time < scene.start_time {
                continue;
            }
            if frame.time > scene.end_time {
                break;
            }
            metrics.total += 1;
            if frame.percentage >= minimum {
                metrics.black += 1;
            }
        }
        metrics
    }

    #[allow(clippy::cast_precision_loss)]
    fn meets_density(&self, minimum_density: f64) -> bool {
        self.total > 0 && self.black as f64 / self.total as f64 >= minimum_density
    }

    #[allow(clippy::cast_precision_loss)]
    fn is_sparse(&self, scene: &CreditScene, minimum_duration: i32) -> bool {
        if self.black <= 1 {
            return true;
        }
        scene.duration() / (self.black - 1) as f64
            > policy::maximum_sparse_average_black_frame_gap(minimum_duration)
    }
}

/// `NormalizeThreshold`: the black and scene-change percentages, lifted by
/// the scan's floor (its 1st-percentile percentage, at most 30) for dark
/// shows. C#'s integer arithmetic.
pub(super) fn normalize_threshold(frames: &[BlackFrame], minimum_percentage: i32) -> (i32, i32) {
    let mut ordered: Vec<i32> = frames.iter().map(|f| f.percentage).collect();
    ordered.sort_unstable();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let index = ((ordered.len() as f64 * 0.01) as usize).min(ordered.len().saturating_sub(1));
    let floor = ordered.get(index).copied().unwrap_or(0).min(30);
    (
        minimum_percentage * (100 - floor) / 100 + floor,
        95 * (100 - floor) / 100 + floor,
    )
}

/// `EstimateMaximumInRunGap`: five median keyframe gaps, at most the merge
/// gap.
fn maximum_in_run_gap(frames: &[BlackFrame]) -> f64 {
    let mut gaps: Vec<f64> = frames
        .windows(2)
        .map(|pair| pair[1].time - pair[0].time)
        .filter(|gap| *gap > 0.0)
        .collect();
    if gaps.is_empty() {
        return policy::MAXIMUM_SCENE_MERGE_GAP;
    }
    gaps.sort_by(f64::total_cmp);
    policy::MAXIMUM_SCENE_MERGE_GAP
        .min(gaps[gaps.len() / 2] * policy::MAXIMUM_KEYFRAME_GAP_MULTIPLIER)
}

/// `DetectCreditSceneCandidates` (`FindRawScenes`): the runs of black
/// keyframes.
pub(super) fn candidates(frames: &[BlackFrame], minimum: i32) -> Vec<CreditScene> {
    let gap = maximum_in_run_gap(frames);
    let mut scenes = Vec::new();
    let mut run: Option<(BlackFrame, BlackFrame)> = None;
    for frame in frames.iter().filter(|f| f.percentage >= minimum) {
        run = Some(match run {
            Some((start, last)) if frame.time - last.time > gap => {
                scenes.push(CreditScene::new(
                    start.frame,
                    last.frame,
                    start.time,
                    last.time,
                ));
                (*frame, *frame)
            }
            Some((start, _)) => (start, *frame),
            None => (*frame, *frame),
        });
    }
    if let Some((start, last)) = run {
        scenes.push(CreditScene::new(
            start.frame,
            last.frame,
            start.time,
            last.time,
        ));
    }
    scenes
}

/// `DetectCreditScenes`: dense runs, merged across short dense gaps, each
/// start moved to its first scene-change keyframe, long enough (or able to
/// become so by boundary refinement).
pub(super) fn credit_scenes(
    frames: &[BlackFrame],
    (minimum, scene_change): (i32, i32),
    minimum_duration: i32,
    allow_boundary_refinement: bool,
) -> Vec<CreditScene> {
    let density = policy::MINIMUM_BLACK_FRAME_DENSITY;
    let dense: Vec<CreditScene> = candidates(frames, minimum)
        .into_iter()
        .filter(|scene| Metrics::of(frames, scene, minimum).meets_density(density))
        .collect();
    let merged = merge_nearby(frames, dense, minimum, density);
    shift_starts(frames, merged, scene_change)
        .into_iter()
        .filter(|scene| {
            scene.duration() >= f64::from(minimum_duration)
                || (allow_boundary_refinement
                    && boundary_keyframes(frames, scene).is_some_and(|(last, _)| {
                        should_refine_boundary(scene, last, minimum_duration)
                    }))
        })
        .collect()
}

/// `MergeNearbyScenes`: neighbours at most the merge gap apart merge when the
/// merged span stays dense.
fn merge_nearby(
    frames: &[BlackFrame],
    scenes: Vec<CreditScene>,
    minimum: i32,
    density: f64,
) -> Vec<CreditScene> {
    let mut scenes = scenes.into_iter();
    let Some(mut current) = scenes.next() else {
        return Vec::new();
    };
    let mut merged = Vec::new();
    for scene in scenes {
        let candidate = CreditScene::new(
            current.start_frame,
            scene.end_frame,
            current.start_time,
            scene.end_time,
        );
        if scene.start_time - current.end_time <= policy::MAXIMUM_SCENE_MERGE_GAP
            && Metrics::of(frames, &candidate, minimum).meets_density(density)
        {
            current = candidate;
        } else {
            merged.push(current);
            current = scene;
        }
    }
    merged.push(current);
    merged
}

/// `ShiftStartsToTransitionFrames`: a scene starts at its first keyframe
/// black enough to be the cut into the credits.
fn shift_starts(
    frames: &[BlackFrame],
    scenes: Vec<CreditScene>,
    scene_change: i32,
) -> Vec<CreditScene> {
    let mut search_start = 0;
    scenes
        .into_iter()
        .map(|scene| {
            let (mut start_frame, mut start_time) = (scene.start_frame, scene.start_time);
            for (i, frame) in frames.iter().enumerate().skip(search_start) {
                if frame.frame > scene.end_frame {
                    break;
                }
                if frame.frame >= start_frame {
                    search_start = search_start.max(i);
                    if frame.percentage >= scene_change {
                        start_frame = frame.frame;
                        start_time = frame.time;
                        break;
                    }
                }
            }
            CreditScene::new(start_frame, scene.end_frame, start_time, scene.end_time)
        })
        .collect()
}

/// `DetectIntervalSupportedCreditScenes`: candidates a `blackdetect` interval
/// supports, spanning the interval's start to the later of the two ends.
pub(super) fn interval_supported(
    frames: &[BlackFrame],
    intervals: &[BlackInterval],
    minimum: i32,
    minimum_duration: i32,
) -> Vec<CreditScene> {
    if intervals.is_empty() {
        return Vec::new();
    }
    candidates(frames, minimum)
        .into_iter()
        .filter_map(|candidate| {
            let interval =
                supporting_interval(candidate.start_time, candidate.end_time, intervals)?;
            let (start, end) = (interval.start, candidate.end_time.max(interval.end));
            (end - start >= f64::from(minimum_duration)).then(|| {
                CreditScene::new(
                    start_frame(frames, &candidate, start, minimum),
                    end_frame(frames, &candidate, end, minimum),
                    start,
                    end,
                )
            })
        })
        .collect()
}

/// `FindSupportingInterval`: of the intervals reaching the candidate, the one
/// giving the longest scene.
fn supporting_interval(
    first: f64,
    last: f64,
    intervals: &[BlackInterval],
) -> Option<BlackInterval> {
    let mut best: Option<(BlackInterval, f64)> = None;
    for interval in intervals {
        if interval.start <= last
            && interval.end >= first - policy::MAXIMUM_INTERVAL_TO_KEYFRAME_GAP
        {
            let span = last.max(interval.end) - interval.start;
            if best.is_none_or(|(_, longest)| span > longest) {
                best = Some((*interval, span));
            }
        }
    }
    best.map(|(interval, _)| interval)
}

/// `FindStartFrame`: the candidate's first black keyframe at or after `time`.
fn start_frame(frames: &[BlackFrame], scene: &CreditScene, time: f64, minimum: i32) -> i64 {
    frames
        .iter()
        .filter(|f| f.frame >= scene.start_frame)
        .take_while(|f| f.frame <= scene.end_frame)
        .find(|f| f.time >= time && f.percentage >= minimum)
        .map_or(scene.end_frame, |f| f.frame)
}

/// `FindEndFrame`: the candidate's last black keyframe at or before `time`.
fn end_frame(frames: &[BlackFrame], scene: &CreditScene, time: f64, minimum: i32) -> i64 {
    frames
        .iter()
        .filter(|f| f.frame >= scene.start_frame)
        .take_while(|f| f.frame <= scene.end_frame)
        .filter(|f| f.time <= time && f.percentage >= minimum)
        .last()
        .map_or(scene.start_frame, |f| f.frame)
}

/// `BuildIntervalProbeRanges`: each candidate padded by a minimum credits
/// duration, clamped to the window, overlapping ranges merged (absolute).
pub(super) fn interval_probe_ranges(
    candidates: &[CreditScene],
    minimum_duration: i32,
    (window_start, window_end): (f64, f64),
) -> Vec<(f64, f64)> {
    let padding = policy::interval_probe_padding(minimum_duration);
    let mut ranges: Vec<(f64, f64)> = candidates
        .iter()
        .map(|c| {
            (
                window_start.max(window_start + c.start_time - padding),
                window_end.min(window_start + c.end_time + padding),
            )
        })
        .filter(|(start, end)| end - start > 0.0)
        .collect();
    ranges.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut merged: Vec<(f64, f64)> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(current) if range.0 <= current.1 => current.1 = current.1.max(range.1),
            _ => merged.push(range),
        }
    }
    merged
}

/// `RankCreditCandidates`: interval-supported scenes first, then the latest.
pub(super) fn rank(scenes: &[CreditScene], intervals: &[BlackInterval]) -> Vec<CreditScene> {
    let mut ranked: Vec<(usize, bool, CreditScene)> = scenes
        .iter()
        .enumerate()
        .map(|(i, scene)| (i, has_interval_support(scene, intervals), *scene))
        .collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.0.cmp(&a.0)));
    ranked.into_iter().map(|(_, _, scene)| scene).collect()
}

/// `HasIntervalSupport`.
fn has_interval_support(scene: &CreditScene, intervals: &[BlackInterval]) -> bool {
    intervals.iter().any(|i| {
        scene.end_time.min(i.end) - scene.start_time.max(i.start)
            >= policy::MINIMUM_INTERVAL_OVERLAP
    })
}

/// `CreditsBoundaryHelper.FindBoundaryKeyframeTimes`: the keyframe before
/// the scene and its first keyframe.
pub(super) fn boundary_keyframes(frames: &[BlackFrame], scene: &CreditScene) -> Option<(f64, f64)> {
    let mut last = None;
    for frame in frames {
        if frame.time >= scene.start_time {
            return last.map(|last| (last, frame.time));
        }
        last = Some(frame.time);
    }
    None
}

/// `CreditsBoundaryHelper.SelectProbeMinimum`: the scene's first keyframe's
/// percentage, at most the scene-change one.
pub(super) fn probe_minimum(frames: &[BlackFrame], scene: &CreditScene, scene_change: i32) -> i32 {
    frames
        .iter()
        .find(|f| f.frame == scene.start_frame)
        .map_or(scene_change, |f| f.percentage.min(scene_change))
}

/// `CreditsBoundaryHelper.ShouldRefineBoundary`: the keyframe gap is worth
/// probing and could make the scene long enough.
pub(super) fn should_refine_boundary(
    scene: &CreditScene,
    last_keyframe: f64,
    minimum_duration: i32,
) -> bool {
    let window = scene.start_time - last_keyframe;
    window > policy::MINIMUM_BOUNDARY_PROBE_WINDOW
        && scene.duration() + window >= f64::from(minimum_duration)
}

/// `CreditsBoundaryHelper.TryRefineBoundaryTime`: a probed black frame
/// strictly after the previous keyframe and not after the scene start.
pub(super) fn refine_boundary_time(
    probe: f64,
    last_keyframe: f64,
    scene_start: f64,
) -> Option<f64> {
    let refined = probe + last_keyframe;
    (refined > last_keyframe && refined <= scene_start).then_some(refined)
}

/// `CreditEntropyFallback`: muted, uniform credit cards.
mod entropy {
    use super::policy;
    use crate::ffmpeg::KeyframeVisual;

    /// Isolated edge cards further than this many dense-body gaps are trimmed.
    const ISOLATED_CARD_TRIM_GAP_MULTIPLIER: f64 = 2.5;
    /// A card's luma entropy is below this.
    const ENTROPY_CREDIT_MAXIMUM: f64 = 0.35;
    /// A card's saturation is below this: cards are muted, never vivid (a
    /// saturated uniform frame is as likely a fade or a sky).
    const SATURATION_CREDIT_MAXIMUM: f64 = 96.0;
    /// The share of a run's keyframes that must be cards.
    const MINIMUM_CARD_FRACTION: f64 = 0.5;

    /// `IsCreditCardKeyframe`.
    pub fn is_card(visual: &KeyframeVisual) -> bool {
        visual.entropy < ENTROPY_CREDIT_MAXIMUM && visual.saturation < SATURATION_CREDIT_MAXIMUM
    }

    /// `FindCreditRange`: the latest qualifying run of cards. A run breaks
    /// only at a gap past the merge gap with a non-card keyframe in it.
    pub fn find_credit_range(
        visuals: &[KeyframeVisual],
        minimum_duration: i32,
    ) -> Option<(f64, f64)> {
        let mut best = None;
        let mut run: Vec<KeyframeVisual> = Vec::new();
        let mut non_card_since_last = false;
        for visual in visuals {
            if !is_card(visual) {
                non_card_since_last = true;
                continue;
            }
            if let Some(last) = run.last()
                && visual.time - last.time > policy::MAXIMUM_SCENE_MERGE_GAP
                && non_card_since_last
            {
                best = latest_qualifying(best, &run, visuals, minimum_duration);
                run.clear();
            }
            run.push(*visual);
            non_card_since_last = false;
        }
        latest_qualifying(best, &run, visuals, minimum_duration)
    }

    /// `SelectLatestQualifyingRun`: the run (edges trimmed) when long enough
    /// and dense with cards, else the best so far.
    fn latest_qualifying(
        best: Option<(f64, f64)>,
        run: &[KeyframeVisual],
        visuals: &[KeyframeVisual],
        minimum_duration: i32,
    ) -> Option<(f64, f64)> {
        if run.is_empty() {
            return best;
        }
        let (start, end) = trim_isolated_ends(run, minimum_duration);
        let (start, end) = (run[start].time, run[end].time);
        if end - start < f64::from(minimum_duration) || !dense_with_cards(visuals, start, end) {
            return best;
        }
        Some((start, end))
    }

    /// `HasSufficientCardDensity`.
    #[allow(clippy::cast_precision_loss)]
    fn dense_with_cards(visuals: &[KeyframeVisual], start: f64, end: f64) -> bool {
        let inside: Vec<&KeyframeVisual> = visuals
            .iter()
            .filter(|v| v.time >= start && v.time <= end)
            .collect();
        let cards = inside.iter().filter(|v| is_card(v)).count();
        !inside.is_empty() && cards as f64 / inside.len() as f64 >= MINIMUM_CARD_FRACTION
    }

    /// `TrimIsolatedEnds`: drop edge cards isolated from the dense body,
    /// unless that leaves less than the minimum duration.
    fn trim_isolated_ends(run: &[KeyframeVisual], minimum_duration: i32) -> (usize, usize) {
        let (mut start, mut end) = (0, run.len() - 1);
        if end < 1 {
            return (start, end);
        }
        let trim_gap = dense_cadence_gap(run) * ISOLATED_CARD_TRIM_GAP_MULTIPLIER;
        while start < end && run[start + 1].time - run[start].time > trim_gap {
            start += 1;
        }
        while end > start && run[end].time - run[end - 1].time > trim_gap {
            end -= 1;
        }
        if run[end].time - run[start].time < f64::from(minimum_duration) {
            return (0, run.len() - 1);
        }
        (start, end)
    }

    /// `DenseCadenceGap`: the lower-quartile card gap.
    fn dense_cadence_gap(run: &[KeyframeVisual]) -> f64 {
        let mut gaps: Vec<f64> = run.windows(2).map(|p| p[1].time - p[0].time).collect();
        gaps.sort_by(f64::total_cmp);
        gaps[gaps.len() / 4]
    }
}

impl DetectSegmentsTask {
    /// `CreditsBlackFrameAnalyzer.AnalyzeMediaFiles`: each item still needing
    /// credits gets them from its credit scenes, adjusted and stored. A
    /// failed detection skips the item.
    pub(super) async fn credit_scene_analyzer(
        &self,
        items: &mut [Item],
        config: &IntroSkipperConfig,
    ) -> Result<(), ServiceError> {
        for item in items.iter_mut().filter(|i| i.needs_analysis(Mode::Credits)) {
            match self.detect_credits(&item.entry, config).await {
                Ok(Some(credits)) if credits.1 > 0.0 => {
                    let snap = config.adjust_intro_based_on_chapters;
                    self.store_found(item, Mode::Credits, credits, config, snap)
                        .await?;
                }
                Ok(_) => {
                    tracing::debug!(item = item.entry.name, "intro skipper: no credit scene");
                }
                Err(err) => {
                    tracing::debug!(%err, item = item.entry.name, "intro skipper: credit scene analysis failed");
                }
            }
        }
        Ok(())
    }

    /// `DetectCreditsAsync`: black credit scenes, else (with
    /// `DetectNonBlackCredits`) muted credit cards; absolute times.
    pub(super) async fn detect_credits(
        &self,
        entry: &QueuedEpisode,
        config: &IntroSkipperConfig,
    ) -> Result<Option<(f64, f64)>, String> {
        let frames = self.keyframe_black_frames(entry, config).await?;
        if !frames.is_empty()
            && let Some(credits) = self.scene_credits(entry, &frames, config).await?
        {
            return Ok(Some(credits));
        }
        if !config.detect_non_black_credits {
            return Ok(None);
        }
        let visuals = self.keyframe_visuals(entry, config).await?;
        let offset = entry.credits_fingerprint_start;
        Ok(
            entropy::find_credit_range(&visuals, config.minimum_credits_duration)
                .map(|(start, end)| (start + offset, end + offset)),
        )
    }

    /// `DetectBlackFrameCreditsAsync`.
    async fn scene_credits(
        &self,
        entry: &QueuedEpisode,
        frames: &[BlackFrame],
        config: &IntroSkipperConfig,
    ) -> Result<Option<(f64, f64)>, String> {
        let minimum_duration = config.minimum_credits_duration;
        let (minimum, scene_change) =
            normalize_threshold(frames, config.black_frame_minimum_percentage);
        let refine = config.refine_credits_boundary;
        let mut scenes = credit_scenes(frames, (minimum, scene_change), minimum_duration, refine);
        let mut intervals = Vec::new();
        if scenes.is_empty() {
            let candidates = candidates(frames, minimum);
            if candidates.is_empty() {
                return Ok(None);
            }
            intervals = self
                .candidate_intervals(entry, &candidates, minimum, config)
                .await;
            scenes = interval_supported(frames, &intervals, minimum, minimum_duration);
            if scenes.is_empty() {
                return Ok(None);
            }
        } else if let [scene] = scenes[..]
            && Metrics::of(frames, &scene, minimum).is_sparse(&scene, minimum_duration)
        {
            intervals = self
                .candidate_intervals(entry, &scenes, minimum, config)
                .await;
            let supported = interval_supported(frames, &intervals, minimum, minimum_duration);
            if !supported.is_empty() {
                scenes = supported;
            }
        }
        let offset = entry.credits_fingerprint_start;
        for scene in rank(&scenes, &intervals) {
            let start = if refine {
                self.refine_start(entry, frames, &scene, scene_change, config)
                    .await?
            } else {
                scene.start_time
            };
            let segment = (start + offset, scene.end_time + offset);
            if segment.1 - segment.0 >= f64::from(minimum_duration) {
                return Ok(Some(segment));
            }
        }
        Ok(None)
    }

    /// `DetectBlackIntervalsForCandidatesOrEmptyAsync`: none when `blackdetect`
    /// fails.
    async fn candidate_intervals(
        &self,
        entry: &QueuedEpisode,
        candidates: &[CreditScene],
        minimum: i32,
        config: &IntroSkipperConfig,
    ) -> Vec<BlackInterval> {
        let end = if entry.credits_fingerprint_end > 0.0 {
            entry.credits_fingerprint_end
        } else {
            entry.duration
        };
        let ranges = interval_probe_ranges(
            candidates,
            config.minimum_credits_duration,
            (entry.credits_fingerprint_start, end),
        );
        let mut intervals = Vec::new();
        for range in ranges {
            match self
                .black_intervals(
                    entry,
                    range,
                    (config.black_frame_threshold, minimum),
                    config,
                )
                .await
            {
                Ok(found) => intervals.extend(found),
                Err(err) => {
                    tracing::debug!(%err, item = entry.name, "intro skipper: black intervals unavailable");
                    return Vec::new();
                }
            }
        }
        intervals
    }

    /// `CreditsBoundaryRefiner.RefineAsync`: the first black frame between
    /// the keyframe before the scene and its first keyframe, when worth it.
    async fn refine_start(
        &self,
        entry: &QueuedEpisode,
        frames: &[BlackFrame],
        scene: &CreditScene,
        scene_change: i32,
        config: &IntroSkipperConfig,
    ) -> Result<f64, String> {
        let Some((last_keyframe, first_black)) = boundary_keyframes(frames, scene) else {
            return Ok(scene.start_time);
        };
        if !should_refine_boundary(scene, last_keyframe, config.minimum_credits_duration) {
            return Ok(scene.start_time);
        }
        let offset = entry.credits_fingerprint_start;
        let probe = self
            .black_frames_at(
                entry,
                Mode::Credits,
                (last_keyframe + offset, first_black + offset),
                probe_minimum(frames, scene, scene_change),
                config,
            )
            .await?;
        Ok(probe
            .first()
            .and_then(|frame| refine_boundary_time(frame.time, last_keyframe, scene.start_time))
            .unwrap_or(scene.start_time))
    }
}

/// `TestBlackFrames` (intro-skipper `db09359`): the credit-scene cases.
#[cfg(test)]
#[allow(clippy::float_cmp)] // exact frame times
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::ffmpeg::{FakeFfmpeg, KeyframeVisual};
    use crate::intro_skipper::tests::{TWO_EPISODES, harness};

    fn bf(percentage: i32, time: f64, frame: i64) -> BlackFrame {
        BlackFrame {
            percentage,
            time,
            frame,
        }
    }

    fn scene(start_frame: i64, end_frame: i64, start_time: f64, end_time: f64) -> CreditScene {
        CreditScene::new(start_frame, end_frame, start_time, end_time)
    }

    fn interval(start: f64, end: f64) -> BlackInterval {
        BlackInterval { start, end }
    }

    fn visual(time: f64, entropy: f64, saturation: f64) -> KeyframeVisual {
        KeyframeVisual {
            time,
            entropy,
            saturation,
        }
    }

    /// `CreateDenseFrames`: a frame every half second from `start` to `end`.
    #[allow(clippy::cast_possible_truncation)]
    fn dense(start: f64, end: f64, percentage: i32, first_frame: Option<i64>) -> Vec<BlackFrame> {
        let mut frame = first_frame.unwrap_or((start * 2.0) as i64);
        let mut frames = Vec::new();
        let mut time = start;
        while time <= end {
            frames.push(bf(percentage, time, frame));
            frame += 1;
            time += 0.5;
        }
        frames
    }

    /// Every `step` from `from` (inclusive) while `keep(time)`.
    fn times(from: f64, step: f64, keep: impl Fn(f64) -> bool) -> Vec<f64> {
        let mut out = Vec::new();
        let mut t = from;
        while keep(t) {
            out.push(t);
            t += step;
        }
        out
    }

    #[test]
    fn scenes_merge_across_a_twenty_second_gap() {
        let mut frames: Vec<BlackFrame> = (0..20)
            .map(|i| bf(95, f64::from(i) * 0.5, i.into()))
            .collect();
        frames.extend((0..10).map(|i| bf(10, 10.0 + f64::from(i) * 2.0, (20 + i).into())));
        frames.extend((0..20).map(|i| bf(95, 29.5 + f64::from(i) * 0.5, (30 + i).into())));
        let scenes = credit_scenes(&frames, (85, 96), 15, true);
        assert_eq!(scenes.len(), 1);
        assert_eq!((scenes[0].start_time, scenes[0].end_time), (0.0, 39.0));
    }

    #[test]
    fn scenes_do_not_merge_across_twenty_and_a_half_seconds() {
        let frames: Vec<BlackFrame> = (0..80)
            .map(|i| {
                bf(
                    if (20..60).contains(&i) { 10 } else { 95 },
                    f64::from(i) * 0.5,
                    i.into(),
                )
            })
            .collect();
        assert_eq!(credit_scenes(&frames, (85, 96), 5, true).len(), 2);
    }

    #[rstest::rstest]
    // NormalizeThreshold_UniformFrames: the floor caps at 30.
    #[case(|_| 90, (89, 96))]
    // NormalizeThreshold_LowFloor
    #[case(|i| if i < 2 { 5 } else { 80 }, (85, 95))]
    fn thresholds_normalize(#[case] percentage: fn(i32) -> i32, #[case] expected: (i32, i32)) {
        let frames: Vec<BlackFrame> = (0..100)
            .map(|i| bf(percentage(i), f64::from(i) * 0.5, i.into()))
            .collect();
        assert_eq!(normalize_threshold(&frames, 85), expected);
    }

    #[rstest::rstest]
    // DensityGating_RejectsLowDensityScene
    #[case(|i| if i % 5 == 0 { 90 } else { 30 }, false)]
    // DensityGating_AcceptsHighDensityScene
    #[case(|i| if i % 5 == 0 { 30 } else { 90 }, true)]
    fn density_gates_scenes(#[case] percentage: fn(i32) -> i32, #[case] accepted: bool) {
        let frames: Vec<BlackFrame> = (0..100)
            .map(|i| bf(percentage(i), f64::from(i) * 0.5, i.into()))
            .collect();
        assert_eq!(
            !credit_scenes(&frames, (85, 96), 15, true).is_empty(),
            accepted
        );
    }

    #[test]
    fn repeated_low_density_scenes_are_rejected_without_interval_support() {
        let mut frames = Vec::new();
        for (start, first) in [(0.0, 0), (60.0, 120), (120.0, 240)] {
            frames.extend((0..60).map(|i| {
                bf(
                    if i % 3 == 0 { 90 } else { 30 },
                    start + f64::from(i) * 0.5,
                    first + i64::from(i),
                )
            }));
        }
        assert!(credit_scenes(&frames, (85, 96), 15, true).is_empty());
    }

    #[test]
    fn density_gating_does_not_merge_across_a_low_density_gap() {
        let frames: Vec<BlackFrame> = (0..70)
            .map(|i| {
                bf(
                    if (16..54).contains(&i) { 10 } else { 95 },
                    f64::from(i) * 0.5,
                    i.into(),
                )
            })
            .collect();
        let scenes = credit_scenes(&frames, (85, 96), 5, true);
        assert_eq!(scenes.len(), 2);
        assert_eq!((scenes[0].start_time, scenes[0].end_time), (0.0, 7.5));
        assert_eq!((scenes[1].start_time, scenes[1].end_time), (27.0, 34.5));
    }

    #[test]
    fn boundary_keyframes_follow_find_boundary_keyframe_times() {
        // NoPriorKeyframe_ReturnsOriginalStart
        let first = [0.0, 0.5, 1.0, 1.5, 2.0, 2.5].map(|t| bf(95, t, 0));
        assert_eq!(boundary_keyframes(&first, &scene(0, 5, 0.0, 2.5)), None);
        // HasPriorKeyframe_ReturnsBoundaryTimes
        let prior = [
            bf(20, 0.0, 0),
            bf(30, 0.5, 1),
            bf(95, 1.0, 2),
            bf(95, 1.5, 3),
            bf(95, 2.0, 4),
        ];
        assert_eq!(
            boundary_keyframes(&prior, &scene(2, 5, 1.0, 2.5)),
            Some((0.5, 1.0))
        );
        // PrecedingKeyframeIsBlack_StillReturnsPrecedingKeyframe
        let dark = [
            bf(10, 0.0, 0),
            bf(90, 5.0, 1),
            bf(88, 10.0, 2),
            bf(95, 15.0, 3),
            bf(95, 20.0, 4),
        ];
        assert_eq!(
            boundary_keyframes(&dark, &scene(3, 5, 15.0, 25.0)),
            Some((10.0, 15.0))
        );
    }

    #[rstest::rstest]
    // UsesLowerOfSceneStartAndSceneChange
    #[case(&[bf(20, 0.0, 0), bf(30, 0.5, 1), bf(92, 1.0, 2), bf(95, 1.5, 3)], scene(2, 3, 1.0, 1.5), 92)]
    // CapsAtSceneChange
    #[case(&[bf(20, 0.0, 0), bf(30, 0.5, 1), bf(99, 1.0, 2), bf(99, 1.5, 3)], scene(2, 3, 1.0, 1.5), 95)]
    // MissingStartFrame_FallsBackToSceneChange
    #[case(&[bf(50, 0.0, 0), bf(60, 0.5, 1)], scene(99, 100, 10.0, 12.0), 95)]
    fn probe_minimums(
        #[case] frames: &[BlackFrame],
        #[case] scene: CreditScene,
        #[case] expected: i32,
    ) {
        assert_eq!(probe_minimum(frames, &scene, 95), expected);
    }

    #[rstest::rstest]
    #[case::small_keyframe_gap(scene(20, 40, 10.4, 30.0), 10.0, false)]
    #[case::cannot_reach_minimum(scene(20, 40, 10.0, 20.0), 8.0, false)]
    #[case::meaningful_window(scene(20, 40, 10.0, 24.0), 8.5, true)]
    fn boundary_refinement_is_worth_it(
        #[case] scene: CreditScene,
        #[case] last: f64,
        #[case] expected: bool,
    ) {
        assert_eq!(should_refine_boundary(&scene, last, 15), expected);
    }

    #[rstest::rstest]
    #[case::at_preceding_keyframe(0.0, None)]
    #[case::inside_window(2.5, Some(12.5))]
    #[case::at_scene_start(5.0, Some(15.0))]
    #[case::after_scene_start(6.0, None)]
    fn boundary_times_refine(#[case] probe: f64, #[case] expected: Option<f64>) {
        assert_eq!(refine_boundary_time(probe, 10.0, 15.0), expected);
    }

    #[test]
    fn interval_supported_scenes() {
        // UsesIntervalEndForDurationAndBounds
        let found = interval_supported(
            &[bf(96, 10.0, 10), bf(96, 12.0, 12)],
            &[interval(5.0, 25.0)],
            85,
            15,
        );
        assert_eq!(
            found
                .iter()
                .map(|s| (s.start_time, s.end_time))
                .collect::<Vec<_>>(),
            [(5.0, 25.0)]
        );
        // AnchorsTailSupportToInterval
        let tail: Vec<BlackFrame> = (0..=10)
            .map(|i| bf(96, f64::from(i * 10), (i * 10).into()))
            .collect();
        let found = interval_supported(&tail, &[interval(90.0, 120.0)], 85, 15);
        assert_eq!(found, [scene(90, 100, 90.0, 120.0)]);
        // PrefersLongerOverlappingInterval
        let found = interval_supported(
            &[bf(96, 10.0, 10), bf(96, 12.0, 11)],
            &[interval(9.0, 13.0), interval(9.0, 40.0)],
            85,
            15,
        );
        assert_eq!(
            found
                .iter()
                .map(|s| (s.start_time, s.end_time))
                .collect::<Vec<_>>(),
            [(9.0, 40.0)]
        );
    }

    #[test]
    fn sparse_scenes_follow_the_average_black_frame_gap() {
        let scene = scene(1, 4, 10.0, 40.0);
        let frames = [
            bf(96, 10.0, 1),
            bf(97, 20.0, 2),
            bf(98, 30.0, 3),
            bf(99, 40.0, 4),
        ];
        let metrics = Metrics::of(&frames, &scene, 85);
        assert_eq!(metrics.black, 4);
        assert!(metrics.meets_density(policy::MINIMUM_BLACK_FRAME_DENSITY));
        assert!(metrics.is_sparse(&scene, 15));
    }

    #[test]
    fn interval_probe_ranges_merge_overlapping_padded_ranges() {
        let ranges = interval_probe_ranges(
            &[
                scene(10, 20, 100.0, 120.0),
                scene(21, 30, 130.0, 150.0),
                scene(80, 90, 300.0, 330.0),
            ],
            15,
            (1000.0, 1400.0),
        );
        assert_eq!(ranges, [(1085.0, 1165.0), (1285.0, 1345.0)]);
    }

    /// `TestRankCreditCandidates_SelectsExpectedScene`.
    #[rstest::rstest]
    #[case::no_intervals_latest_scene(2, &[], 1)]
    #[case::interval_promotes_earlier_scene(2, &[interval(205.0, 246.0)], 0)]
    #[case::sub_threshold_overlap_is_not_support(2, &[interval(259.9, 280.0)], 1)]
    #[case::interval_supports_later_scene(2, &[interval(315.0, 360.0)], 1)]
    #[case::latest_supported_beats_unsupported_later(3, &[interval(55.0, 95.0), interval(155.0, 195.0)], 1)]
    fn credit_candidates_rank(
        #[case] count: usize,
        #[case] intervals: &[BlackInterval],
        #[case] expected: usize,
    ) {
        let scenes = if count == 2 {
            vec![scene(400, 520, 200.0, 260.0), scene(620, 700, 310.0, 350.0)]
        } else {
            vec![
                scene(100, 200, 50.0, 90.0),
                scene(300, 400, 150.0, 190.0),
                scene(500, 600, 250.0, 290.0),
            ]
        };
        assert_eq!(rank(&scenes, intervals)[0], scenes[expected]);
    }

    #[rstest::rstest]
    #[case(0.12, 30.0, true)]
    #[case(0.349, 95.0, true)]
    #[case(0.35, 30.0, false)]
    #[case(0.55, 30.0, false)]
    #[case(0.12, 96.0, false)]
    #[case(0.12, 200.0, false)]
    fn credit_card_keyframes(
        #[case] entropy: f64,
        #[case] saturation: f64,
        #[case] expected: bool,
    ) {
        assert_eq!(
            entropy::is_card(&visual(0.0, entropy, saturation)),
            expected
        );
    }

    const CARD: (f64, f64) = (0.12, 30.0);
    const BUSY: (f64, f64) = (0.55, 108.0);

    fn at(times: &[f64], look: (f64, f64)) -> Vec<KeyframeVisual> {
        times.iter().map(|t| visual(*t, look.0, look.1)).collect()
    }

    /// `CreateCardCreditVisuals`: content every 2 s, then cards to `end`.
    fn card_credits(start: f64, end: f64, saturation: f64) -> Vec<KeyframeVisual> {
        let mut visuals = at(&times(0.0, 2.0, |t| t < start), (0.53, 108.0));
        visuals.extend(at(&times(start, 2.0, |t| t <= end), (0.15, saturation)));
        visuals
    }

    #[test]
    fn entropy_fallback_runs() {
        let range = |v: &[KeyframeVisual], minimum| entropy::find_credit_range(v, minimum);
        // LaterSparseCreditsNotConstrainedByEarlierDenseRun
        let mut v = at(&times(0.0, 2.0, |t| t <= 20.0), CARD);
        v.extend(at(&times(22.0, 2.0, |t| t < 60.0), BUSY));
        v.extend(at(&[60.0, 72.0, 84.0, 96.0], CARD));
        assert_eq!(range(&v, 15), Some((60.0, 96.0)));
        // KeepsAllCardRunWhenGopExceedsBridge
        assert_eq!(
            range(&at(&[0.0, 21.0, 42.0, 63.0], CARD), 60),
            Some((0.0, 63.0))
        );
        // GroupsSparseCardsAfterDenseContent
        let mut v = at(&times(0.0, 2.0, |t| t <= 58.0), BUSY);
        v.extend(at(&[60.0, 72.0, 84.0, 96.0], CARD));
        assert_eq!(range(&v, 15), Some((60.0, 96.0)));
        // TrimsSparseTailPastSubstantialDenseBody
        let mut v = at(&times(0.0, 2.0, |t| t <= 20.0), CARD);
        v.extend(at(&times(28.0, 8.0, |t| t <= 196.0), CARD));
        assert_eq!(range(&v, 15), Some((0.0, 20.0)));
        // RejectsIsolatedCardsBridgingBusyContent
        let v: Vec<KeyframeVisual> = times(0.0, 2.0, |t| t <= 18.0)
            .into_iter()
            .map(|t| {
                if t == 0.0 || t == 18.0 {
                    visual(t, CARD.0, CARD.1)
                } else {
                    visual(t, BUSY.0, BUSY.1)
                }
            })
            .collect();
        assert_eq!(range(&v, 15), None);
        // DetectsLowEntropyCardRun
        assert_eq!(
            range(&card_credits(30.0, 54.0, 32.0), 15),
            Some((30.0, 54.0))
        );
        // RejectsHighEntropyDarkScene
        assert_eq!(
            range(&at(&times(0.0, 2.0, |t| t < 60.0), (0.63, 50.0)), 15),
            None
        );
        assert_eq!(range(&card_credits(0.0, 20.0, 200.0), 15), None);
        // RejectsSubMinimumDurationRun
        assert_eq!(range(&card_credits(30.0, 40.0, 32.0), 15), None);
        // SelectsLatestQualifyingRun
        let mut v = at(&times(0.0, 2.0, |t| t <= 20.0), CARD);
        v.extend(at(&times(22.0, 2.0, |t| t < 60.0), BUSY));
        v.extend(at(&times(60.0, 2.0, |t| t <= 80.0), CARD));
        assert_eq!(range(&v, 15), Some((60.0, 80.0)));
    }

    /// `TestCreditEntropyFallback_TrailingTrimBracket` (with
    /// `TrimsOverExtendedTail` and `TrimsIsolatedLeadingCard`).
    #[rstest::rstest]
    #[case(58.0, 2.0, &[(0.0, 20.0), (30.0, 30.0), (38.0, 38.0), (46.0, 46.0), (54.0, 54.0)], Some((0.0, 20.0)))]
    #[case(88.0, 4.0, &[(36.0, 36.0), (52.0, 80.0)], Some((52.0, 80.0)))]
    #[case(54.0, 2.0, &[(30.0, 54.0)], Some((30.0, 54.0)))]
    #[case(60.0, 2.0, &[(0.0, 28.0), (36.0, 60.0)], Some((0.0, 60.0)))]
    #[case(60.0, 2.0, &[(0.0, 48.0), (56.0, 60.0)], Some((0.0, 60.0)))]
    #[case(40.0, 8.0, &[(0.0, 40.0)], Some((0.0, 40.0)))]
    #[case(48.0, 12.0, &[(0.0, 48.0)], Some((0.0, 48.0)))]
    #[case(80.0, 2.0, &[(0.0, 20.0), (60.0, 80.0)], Some((60.0, 80.0)))]
    #[case(54.0, 2.0, &[(0.0, 6.0), (14.0, 14.0), (22.0, 22.0), (30.0, 30.0), (38.0, 38.0), (46.0, 46.0), (54.0, 54.0)], None)]
    #[case(44.0, 2.0, &[(0.0, 40.0), (44.0, 44.0)], Some((0.0, 44.0)))]
    #[case(60.0, 2.0, &[], None)]
    fn entropy_trailing_trim_bracket(
        #[case] end: f64,
        #[case] step: f64,
        #[case] cards: &[(f64, f64)],
        #[case] expected: Option<(f64, f64)>,
    ) {
        let v: Vec<KeyframeVisual> = times(0.0, step, |t| t <= end + 1e-9)
            .into_iter()
            .map(|t| {
                let card = cards
                    .iter()
                    .any(|(from, to)| t >= from - 1e-9 && t <= to + 1e-9);
                let look = if card { CARD } else { BUSY };
                visual(t, look.0, look.1)
            })
            .collect();
        assert_eq!(entropy::find_credit_range(&v, 15), expected);
    }

    /// `TestFingerprint_Alt*`: real credits scans (`testdata/`).
    #[rstest::rstest]
    #[case::alt3_clean_credits(include_str!("testdata/blackframe-alt-3"), (85, 95), &[(516.012, 584.33)], false)]
    #[case::alt4_dark_show(include_str!("testdata/blackframe-alt-4"), (85, 95), &[(463.425, 558.479)], true)]
    #[case::alt5_mid_credit_scene(include_str!("testdata/blackframe-alt-5"), (88, 96), &[(609.328, 637.12), (725.12, 853.12)], false)]
    fn fingerprint_scans(
        #[case] raw: &str,
        #[case] thresholds: (i32, i32),
        #[case] expected: &[(f64, f64)],
        #[case] last_only: bool,
    ) {
        let frames = crate::ffmpeg::parse_black_frames(raw);
        assert_eq!(normalize_threshold(&frames, 85), thresholds);
        let scenes: Vec<(f64, f64)> = credit_scenes(&frames, thresholds, 15, true)
            .iter()
            .map(|s| (s.start_time, s.end_time))
            .collect();
        if last_only {
            assert!(scenes.len() >= 4, "{scenes:?}");
            assert_eq!(scenes[scenes.len() - 1..], *expected);
        } else {
            assert_eq!(scenes, expected);
        }
    }

    /// `CreateStingerSplitFrames`.
    fn stinger_split() -> Vec<BlackFrame> {
        let mut frames = dense(0.0, 20.0, 95, None);
        frames.extend(dense(20.5, 89.5, 30, Some(41)));
        frames.extend(dense(90.0, 120.0, 95, Some(180)));
        frames
    }

    /// `CreateLowDensitySingleCandidateFrames`.
    fn low_density() -> Vec<BlackFrame> {
        (0..100)
            .map(|i| {
                bf(
                    if i % 3 == 0 { 90 } else { 30 },
                    f64::from(i) * 0.5,
                    i.into(),
                )
            })
            .collect()
    }

    /// One `DetectCreditsAsync` against the fake: the result (absolute) and
    /// the scans as `(kind, start, end)`. Intervals are given relative to the
    /// credits window, as upstream's fake returns them.
    async fn detect(
        fake: FakeFfmpeg,
        credits_start: f64,
        configure: impl FnOnce(&mut IntroSkipperConfig),
    ) -> (Option<(f64, f64)>, Vec<(&'static str, f64, f64, i32)>) {
        let h = harness(&TWO_EPISODES, true, "{}", false).await;
        let fake = Arc::new(FakeFfmpeg {
            intervals: fake
                .intervals
                .iter()
                .map(|i| interval(i.start + credits_start, i.end + credits_start))
                .collect(),
            ..fake
        });
        let task = DetectSegmentsTask {
            ffmpeg: Arc::clone(&fake) as Arc<dyn crate::ffmpeg::FfmpegService>,
            ..h.task.clone()
        };
        let mut config = IntroSkipperConfig {
            cache_fingerprints: false,
            ..IntroSkipperConfig::default()
        };
        configure(&mut config);
        let queued = task.queue(&config).await.expect("queue");
        let entry = QueuedEpisode {
            duration: credits_start + 1800.0,
            credits_fingerprint_start: credits_start,
            credits_fingerprint_end: credits_start + 1800.0,
            ..queued[0].1[0].clone()
        };
        let found = task.detect_credits(&entry, &config).await.expect("detect");
        let scans = fake
            .log
            .lock()
            .expect("log")
            .iter()
            .map(|c| (c.0, c.2, c.3, c.4))
            .collect();
        (found, scans)
    }

    fn count(scans: &[(&str, f64, f64, i32)], kind: &str) -> usize {
        scans.iter().filter(|s| s.0 == kind).count()
    }

    fn frames(keyframe_frames: Vec<BlackFrame>) -> FakeFfmpeg {
        FakeFfmpeg {
            keyframe_frames,
            ..FakeFfmpeg::default()
        }
    }

    #[tokio::test]
    async fn detect_credits_with_black_scenes() {
        // EmptyScan_ReturnsNull
        let (found, scans) = detect(frames(Vec::new()), 0.0, |c| {
            c.detect_non_black_credits = false;
        })
        .await;
        assert_eq!(found, None);
        assert_eq!(
            (
                count(&scans, "keyframe-blackframes"),
                count(&scans, "intervals"),
                count(&scans, "blackframe")
            ),
            (1, 0, 0)
        );
        // SingleCleanScene_ReturnsOffsetSegment
        let (found, scans) = detect(frames(dense(0.0, 20.0, 95, None)), 100.0, |_| ()).await;
        assert_eq!(found, Some((100.0, 120.0)));
        assert_eq!(
            (count(&scans, "intervals"), count(&scans, "blackframe")),
            (0, 0)
        );
        // TooShortScene_ReturnsNull
        let (found, _) = detect(frames(dense(0.0, 10.0, 95, None)), 0.0, |c| {
            c.detect_non_black_credits = false;
        })
        .await;
        assert_eq!(found, None);
        // StingerSplit_ReturnsFinalScene
        let (found, _) = detect(frames(stinger_split()), 1000.0, |_| ()).await;
        assert_eq!(found, Some((1090.0, 1120.0)));
        // ValidBlackFrameSceneSkipsIntervalPromotion
        let fake = FakeFfmpeg {
            intervals: vec![interval(5.0, 10.0)],
            ..frames(stinger_split())
        };
        let (found, scans) = detect(fake, 1000.0, |_| ()).await;
        assert_eq!(
            (found, count(&scans, "intervals")),
            (Some((1090.0, 1120.0)), 0)
        );
        // ValidBlackFrameSceneDoesNotRequireBlackIntervals
        let fake = FakeFfmpeg {
            fail_intervals: true,
            ..frames(stinger_split())
        };
        let (found, scans) = detect(fake, 1000.0, |_| ()).await;
        assert_eq!(
            (found, count(&scans, "intervals")),
            (Some((1090.0, 1120.0)), 0)
        );
    }

    #[tokio::test]
    async fn detect_credits_with_black_intervals() {
        // DarkLowDensityScene_ReturnsNull
        let dark: Vec<BlackFrame> = (0..100)
            .map(|i| {
                bf(
                    if i % 5 == 0 { 95 } else { 30 },
                    f64::from(i) * 0.5,
                    i.into(),
                )
            })
            .collect();
        let (found, scans) =
            detect(frames(dark), 0.0, |c| c.detect_non_black_credits = false).await;
        assert_eq!((found, count(&scans, "intervals")), (None, 1));
        // LowDensitySingleCandidateUsesIntervalSupport
        let fake = FakeFfmpeg {
            intervals: vec![interval(1.0, 49.0)],
            ..frames(low_density())
        };
        let (found, scans) = detect(fake, 0.0, |_| ()).await;
        assert!(found.is_some());
        let probes: Vec<_> = scans.iter().filter(|s| s.0 == "intervals").collect();
        assert_eq!(probes, [&("intervals", 0.0, 64.5, 28)]);
        // LowDensitySingleCandidateWithoutIntervalSupportReturnsNull
        let (found, scans) = detect(frames(low_density()), 0.0, |c| {
            c.detect_non_black_credits = false;
        })
        .await;
        assert_eq!((found, count(&scans, "intervals")), (None, 1));
        // BlackIntervalsRecoverSparseKeyframeCredits
        let sparse = vec![
            bf(15, 366.45, 36),
            bf(96, 376.46, 37),
            bf(96, 386.47, 38),
            bf(98, 396.48, 39),
            bf(99, 406.49, 40),
            bf(20, 416.5, 41),
        ];
        let fake = FakeFfmpeg {
            intervals: vec![interval(367.827, 376.002)],
            ..frames(sparse)
        };
        let (found, scans) = detect(fake, 2356.27, |_| ()).await;
        let (start, end) = found.expect("credits");
        assert!(
            (2724.096..=2724.098).contains(&start) && (2762.759..=2762.761).contains(&end),
            "{start} {end}"
        );
        assert_eq!(count(&scans, "intervals"), 1);
        // SparseSingleSceneWithoutIntervalSupportIsStillReturned
        let sparse = vec![
            bf(10, 0.0, 0),
            bf(96, 10.0, 1),
            bf(96, 20.0, 2),
            bf(96, 30.0, 3),
            bf(96, 40.0, 4),
            bf(10, 50.0, 5),
        ];
        let (found, scans) =
            detect(frames(sparse), 0.0, |c| c.refine_credits_boundary = false).await;
        assert_eq!((found, count(&scans, "intervals")), (Some((10.0, 40.0)), 1));
        // BlackIntervalsExpandSingleShortScene
        let short: Vec<BlackFrame> = (0..6)
            .map(|i| bf(96, 10.0 + f64::from(i) * 2.0, 10 + i64::from(i)))
            .collect();
        let fake = FakeFfmpeg {
            intervals: vec![interval(5.0, 19.8)],
            ..frames(short)
        };
        let (found, scans) = detect(fake, 100.0, |_| ()).await;
        assert_eq!(
            (found, count(&scans, "intervals")),
            (Some((105.0, 120.0)), 1)
        );
        // BlackIntervalsWithoutBlackframeSupportReturnNull
        let unsupported = vec![
            bf(15, 366.45, 36),
            bf(20, 376.46, 37),
            bf(18, 386.47, 38),
            bf(22, 396.48, 39),
        ];
        let fake = FakeFfmpeg {
            intervals: vec![interval(367.827, 376.002)],
            ..frames(unsupported)
        };
        let (found, scans) = detect(fake, 2356.27, |c| c.detect_non_black_credits = false).await;
        assert_eq!((found, count(&scans, "intervals")), (None, 0));
    }

    #[tokio::test]
    async fn detect_credits_refines_the_boundary() {
        let mut lead_in = dense(0.0, 8.0, 30, None);
        lead_in.extend(dense(10.0, 30.0, 95, Some(20)));
        let with_probe = |frames: Vec<BlackFrame>, probe: Vec<BlackFrame>| FakeFfmpeg {
            keyframe_frames: frames,
            range_frames: Some(probe),
            ..FakeFfmpeg::default()
        };
        // RefinesBoundaryByDefault: the probe runs between the keyframes
        // around the cut (absolute 108–110).
        let (found, scans) = detect(
            with_probe(lead_in.clone(), vec![bf(95, 1.25, 0)]),
            100.0,
            |c| c.black_frame_threshold = 32,
        )
        .await;
        assert_eq!(found, Some((109.25, 130.0)));
        let probes: Vec<_> = scans.iter().filter(|s| s.0 == "blackframe").collect();
        assert_eq!(probes, [&("blackframe", 108.0, 110.0, 32)]);
        // A probe frame below the scene's start percentage is not black
        // enough (the minimum is 95 here).
        let (found, _) = detect(
            with_probe(lead_in.clone(), vec![bf(94, 1.25, 0)]),
            100.0,
            |_| (),
        )
        .await;
        assert_eq!(found, Some((110.0, 130.0)));
        // RefinesSubMinimumFinalSceneBeforeSelectingEarlierScene
        let mut split = dense(0.0, 20.0, 95, None);
        split.extend(dense(20.5, 58.0, 30, None));
        split.extend(dense(60.0, 74.0, 95, Some(120)));
        let (found, scans) = detect(with_probe(split, vec![bf(95, 0.5, 0)]), 100.0, |_| ()).await;
        assert_eq!(
            (found, count(&scans, "blackframe")),
            (Some((158.5, 174.0)), 1)
        );
        // DisabledBoundaryRefinement_UsesKeyframeStart
        let (found, scans) = detect(with_probe(lead_in, vec![bf(95, 1.25, 0)]), 100.0, |c| {
            c.refine_credits_boundary = false;
        })
        .await;
        assert_eq!(
            (found, count(&scans, "blackframe")),
            (Some((110.0, 130.0)), 0)
        );
        // DisabledRefinement_DoesNotSuppressIntervalFallback
        let mut gap = dense(0.0, 8.0, 30, None);
        gap.extend(dense(14.0, 24.0, 95, Some(40)));
        let fake = FakeFfmpeg {
            intervals: vec![interval(8.0, 24.0)],
            ..frames(gap)
        };
        let (found, scans) = detect(fake, 100.0, |c| c.refine_credits_boundary = false).await;
        assert_eq!(
            (found, count(&scans, "intervals")),
            (Some((108.0, 124.0)), 1)
        );
    }

    #[tokio::test]
    async fn detect_credits_falls_back_to_credit_cards() {
        let cards = || card_credits(30.0, 54.0, 32.0);
        // NonBlackCreditsFallback_DetectsCardCredits
        let fake = FakeFfmpeg {
            visuals: cards(),
            ..frames(dense(0.0, 54.0, 0, None))
        };
        let (found, scans) = detect(fake, 100.0, |_| ()).await;
        assert_eq!(found, Some((130.0, 154.0)));
        assert_eq!(
            (count(&scans, "visuals"), count(&scans, "intervals")),
            (1, 0)
        );
        // NoBlackFramesAtAll_RunsFallback
        let fake = FakeFfmpeg {
            visuals: cards(),
            ..FakeFfmpeg::default()
        };
        let (found, scans) = detect(fake, 100.0, |_| ()).await;
        assert_eq!((found, count(&scans, "visuals")), (Some((130.0, 154.0)), 1));
        // IntervalMissThenFallback_RecoversNonBlackCredits
        let fake = FakeFfmpeg {
            visuals: cards(),
            ..frames(low_density())
        };
        let (found, scans) = detect(fake, 100.0, |_| ()).await;
        assert_eq!(found, Some((130.0, 154.0)));
        assert_eq!(
            (count(&scans, "intervals"), count(&scans, "visuals")),
            (1, 1)
        );
        // BlackCreditsPresent_DoesNotRunFallback
        let fake = FakeFfmpeg {
            visuals: card_credits(0.0, 20.0, 32.0),
            ..frames(dense(0.0, 20.0, 95, None))
        };
        let (found, scans) = detect(fake, 100.0, |_| ()).await;
        assert_eq!(
            (found.map(|f| f.0), count(&scans, "visuals")),
            (Some(100.0), 0)
        );
        // NonBlackCreditsDisabled_SkipsFallback / NoBlackFramesAtAll_DisabledSkipsFallback
        for keyframe_frames in [dense(0.0, 54.0, 0, None), Vec::new()] {
            let fake = FakeFfmpeg {
                visuals: cards(),
                ..frames(keyframe_frames)
            };
            let (found, scans) = detect(fake, 0.0, |c| c.detect_non_black_credits = false).await;
            assert_eq!((found, count(&scans, "visuals")), (None, 0));
        }
    }

    /// `TestDetectKeyframeVisuals_ClipsScanToCreditsWindow`: visuals past the
    /// credits window (ffmpeg's `-skip_frame nokey` scan overruns `-to`) are
    /// dropped, so credits never land past it.
    #[tokio::test]
    async fn keyframe_visuals_are_clipped_to_the_credits_window() {
        let mut visuals = card_credits(30.0, 54.0, 32.0);
        visuals.push(visual(100.0, BUSY.0, BUSY.1));
        visuals.extend(at(&times(1810.0, 2.0, |t| t <= 1850.0), CARD));
        let fake = FakeFfmpeg {
            visuals,
            ..FakeFfmpeg::default()
        };
        let (found, _) = detect(fake, 100.0, |_| ()).await;
        assert_eq!(found, Some((130.0, 154.0)));
    }

    /// `CreditsBlackFrameAnalyzer.AnalyzeMediaFiles` through the task: chosen
    /// by `UseAlternativeBlackFrameAnalyzer`, it stores each item's credits
    /// (adjusted), skips one whose credits are the user's
    /// (`SkipsAlreadyAnalyzedEpisodes`), and goes on past a failed scan
    /// (`DetectionException_Continues`).
    #[tokio::test]
    async fn the_alternative_analyzer_runs_in_the_chain() {
        use ferrofin_core::ScheduledTask as _;
        let config = r#"{"UseAlternativeBlackFrameAnalyzer":true,"CacheFingerprints":false}"#;
        let (ep_a, ep_b) = (TWO_EPISODES[0].0, TWO_EPISODES[1].0);
        // One fresh pass (analysed items are not analysed again): the
        // keyframe scans made, and each episode's credits.
        let pass = |fake: FakeFfmpeg| async move {
            let h = harness(&TWO_EPISODES, true, config, false).await;
            h.task
                .actions
                .update_timestamp(ferrofin_traits::intro_skipper::StoredSegment {
                    item_id: ep_a,
                    mode: Mode::Credits,
                    start: 1500.0,
                    end: 1600.0,
                    is_user_provided: true,
                    config_hash: String::new(),
                })
                .await
                .expect("user credits");
            let fake = Arc::new(fake);
            let task = DetectSegmentsTask {
                ffmpeg: Arc::clone(&fake) as Arc<dyn crate::ffmpeg::FfmpegService>,
                ..h.task.clone()
            };
            task.execute(&ferrofin_core::TaskProgress::default())
                .await
                .expect("run");
            let scanned: Vec<String> = fake
                .log
                .lock()
                .expect("log")
                .iter()
                .filter(|c| c.0 == "keyframe-blackframes")
                .map(|c| c.1.clone())
                .collect();
            let mut credits = Vec::new();
            for id in [ep_a, ep_b] {
                credits.push(
                    task.actions
                        .segments(id)
                        .await
                        .expect("tier")
                        .into_iter()
                        .find(|s| s.mode == Mode::Credits)
                        .map(|s| (s.start, s.end, s.is_user_provided)),
                );
            }
            let parsed: IntroSkipperConfig = serde_json::from_str(config).expect("config");
            let window_start =
                task.queue(&parsed).await.expect("queue")[0].1[1].credits_fingerprint_start;
            (scanned, credits, window_start)
        };
        // A failed scan stores nothing and stops nothing; the user's credits
        // are not scanned for.
        let (scanned, credits, _) = pass(FakeFfmpeg {
            fail_keyframe_scan: true,
            ..FakeFfmpeg::default()
        })
        .await;
        assert_eq!(scanned.len(), 1, "{scanned:?}");
        assert!(scanned[0].ends_with("s01e02.mkv"));
        assert_eq!(credits, [Some((1500.0, 1600.0, true)), None]);
        // Black from 350 s into the credits window: credits, adjusted (the
        // end snaps to the episode's).
        let (scanned, credits, window_start) = pass(FakeFfmpeg {
            keyframe_frames: dense(350.0, 448.0, 95, None),
            ..FakeFfmpeg::default()
        })
        .await;
        assert_eq!(scanned.len(), 1);
        assert_eq!(
            credits,
            [
                Some((1500.0, 1600.0, true)),
                Some((window_start + 350.0, 1800.0, false))
            ]
        );
    }
}
