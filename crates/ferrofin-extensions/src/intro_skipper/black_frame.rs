//! Black-frame detection — port of the plugin's `BlackFrameAnalyzer` and
//! `RecapDetectionHelper`, and the recap halves of `ChapterAnalyzer`
//! (`DetectRecapUsingBlackFramesAsync`, `BuildRecapFromBlackFrames`) and
//! `ChromaprintAnalyzer` (`BuildRecapFromChromaprintCandidateAsync`)
//! (intro-skipper `db09359`).
//!
//! Credits on a black card are found by bisecting the episode's tail for the
//! first black frame; a recap runs from the start to the last black frame
//! before the intro (or the recap detection limit).

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use ferrofin_model::intro_skipper::AnalysisMode as Mode;
use ferrofin_traits::error::ServiceError;
use ferrofin_traits::intro_skipper as intro_store;
use uuid::Uuid;

use super::engine::Item;
use super::queue::QueuedEpisode;
use super::{DetectSegmentsTask, IntroSkipperConfig, chapter_analyzer};
use crate::ffmpeg::BlackFrame;

/// `_maximumError`: the bisection stops when its bounds are this close.
const MAXIMUM_ERROR: f64 = 4.0;

/// `BuildRecapFromBlackFrames`: from the start to the latest black frame
/// between `minimum` and `maximum`.
pub(super) fn recap_from_black_frames(
    frames: &[BlackFrame],
    minimum: f64,
    maximum: f64,
) -> Option<(f64, f64)> {
    let mut latest: Option<f64> = None;
    for frame in frames {
        if frame.time < minimum || frame.time > maximum {
            continue;
        }
        if latest.is_none_or(|t| frame.time > t) {
            latest = Some(frame.time);
        }
    }
    latest.map(|t| (0.0, t))
}

impl DetectSegmentsTask {
    /// `BlackFrameAnalyzer.AnalyzeMediaFiles` (credits only): each item still
    /// needing credits gets them from a chapter that starts on black
    /// (`UseChapterMarkersBlackFrame`), else by bisection, starting where the
    /// previous item's credits did. Stored as found (no time adjustment, as
    /// upstream). A failed detection skips the item.
    pub(super) async fn black_frame_analyzer(
        &self,
        items: &mut [Item],
        config: &IntroSkipperConfig,
    ) -> Result<(), ServiceError> {
        let minimum = f64::from(config.minimum_credits_duration);
        let mut search_start = 0.0;
        for item in items.iter_mut().filter(|i| i.needs_analysis(Mode::Credits)) {
            let entry = item.entry.clone();
            let credits = match self
                .black_frame_credits(&entry, &mut search_start, config)
                .await
            {
                Ok(credits) => credits,
                Err(err) => {
                    tracing::debug!(%err, item = entry.name, "intro skipper: black frame analysis failed");
                    continue;
                }
            };
            let Some((start, end)) = credits.filter(|(_, end)| *end > 0.0) else {
                tracing::debug!(
                    item = entry.name,
                    "intro skipper: no credits from black frames"
                );
                continue;
            };
            self.store_segment(item, Mode::Credits, (start, end))
                .await?;
            // The next item's credits are searched for around these.
            search_start = entry.duration - start + minimum;
        }
        Ok(())
    }

    /// One item's credits. `search_start` (the distance from the end the
    /// bisection starts at) is reset and found as upstream, and kept so even
    /// when a later detection fails.
    async fn black_frame_credits(
        &self,
        entry: &QueuedEpisode,
        search_start: &mut f64,
        config: &IntroSkipperConfig,
    ) -> Result<Option<(f64, f64)>, String> {
        if config.use_chapter_markers_black_frame
            && let Some(credits) = self.credits_from_chapters(entry, config).await?
        {
            return Ok(Some(credits));
        }
        // A longer previous item's start can lie beyond this one's window,
        // which would cross the bisection's bounds.
        if *search_start > entry.duration - entry.credits_fingerprint_start {
            *search_start = 0.0;
        }
        if *search_start < f64::from(config.minimum_credits_duration) {
            *search_start = self.find_search_start(entry, config).await?;
        }
        self.bisect_credits(entry, *search_start, config).await
    }

    /// `TryAnalyzeChaptersAsync`: the last chapter in the credits window that
    /// starts on black, with no black four seconds before it.
    async fn credits_from_chapters(
        &self,
        entry: &QueuedEpisode,
        config: &IntroSkipperConfig,
    ) -> Result<Option<(f64, f64)>, String> {
        let latest = entry.duration - f64::from(config.minimum_credits_duration);
        let mut starts: Vec<f64> = self
            .item_chapters(entry.episode_id)
            .await
            .iter()
            .map(|c| chapter_analyzer::seconds(c.start_position_ticks))
            .filter(|s| *s >= entry.credits_fingerprint_start && *s <= latest)
            .collect();
        starts.sort_by(|a, b| b.total_cmp(a));
        for start in starts {
            let black = |window| self.black_frames(entry, Mode::Credits, window, config);
            if black((start, start + 1.0)).await?.is_empty() {
                break;
            }
            if black((start - 5.0, start - 4.0)).await?.is_empty() {
                return Ok(Some((start, entry.duration)));
            }
        }
        Ok(None)
    }

    /// `FindSearchStartAsync`: the first distance from the end (in steps of
    /// two minimum credits) whose last second is not mostly black.
    async fn find_search_start(
        &self,
        entry: &QueuedEpisode,
        config: &IntroSkipperConfig,
    ) -> Result<f64, String> {
        let minimum = f64::from(config.minimum_credits_duration);
        let furthest = entry.duration - entry.credits_fingerprint_start;
        let mut distance = 3.0 * minimum;
        while distance < furthest {
            let scan = entry.duration - distance;
            if self
                .black_frames(entry, Mode::Credits, (scan - 1.0, scan), config)
                .await?
                .len()
                < 3
            {
                return Ok(distance);
            }
            distance += 2.0 * minimum;
        }
        Ok(furthest)
    }

    /// `AnalyzeMediaFileAsync`: bisect the distance from the end for the
    /// earliest two-second window holding a black frame, widening a bound
    /// the search runs into.
    pub(super) async fn bisect_credits(
        &self,
        entry: &QueuedEpisode,
        initial: f64,
        config: &IntroSkipperConfig,
    ) -> Result<Option<(f64, f64)>, String> {
        let minimum = f64::from(config.minimum_credits_duration);
        let distance = 2.0 * minimum;
        let furthest = entry.duration - entry.credits_fingerprint_start;
        let mut upper = initial.min(furthest);
        let mut lower = (initial - distance).max(minimum);
        let (mut start, mut end) = (upper, lower);
        let mut first_black = None;
        while start - end > MAXIMUM_ERROR {
            let midpoint = f64::midpoint(start, end);
            let scan = entry.duration - midpoint;
            let frames = self
                .black_frames(entry, Mode::Credits, (scan, scan + 2.0), config)
                .await?;
            if let Some(frame) = frames.first() {
                end = midpoint;
                first_black = Some(frame.time + scan);
                if upper - midpoint < MAXIMUM_ERROR {
                    upper = (upper + 0.5 * distance).min(furthest);
                    start = upper;
                }
            } else {
                start = midpoint - 2.0;
                if midpoint - lower < MAXIMUM_ERROR {
                    lower = (lower - 0.5 * distance).max(minimum);
                    end = lower;
                }
            }
        }
        Ok(first_black
            .filter(|t| *t > 0.0)
            .map(|t| (t, entry.duration)))
    }

    /// `RecapDetectionHelper.GetMaximumBoundaryAsync`: a recap ends before the
    /// recap detection limit and before the item's intro.
    pub(super) async fn recap_boundary(
        &self,
        entry: &QueuedEpisode,
        config: &IntroSkipperConfig,
    ) -> Result<f64, ServiceError> {
        let mut boundary = entry
            .duration
            .min(f64::from(config.maximum_recap_detection_duration));
        let segments = self.actions.segments(entry.episode_id).await?;
        if let Some(intro) = intro_store::timestamps(&segments)
            .get(&Mode::Introduction)
            .filter(|intro| intro.end > 0.0)
        {
            boundary = boundary.min(intro.start);
        }
        Ok(boundary)
    }

    /// `DetectRecapUsingBlackFramesAsync`: the chapter analyzer's recap when
    /// no chapter names one.
    pub(super) async fn recap_from_black_frames(
        &self,
        entry: &QueuedEpisode,
        config: &IntroSkipperConfig,
    ) -> Result<Option<(f64, f64)>, ServiceError> {
        let boundary = self.recap_boundary(entry, config).await?;
        if boundary <= 0.0 {
            return Ok(None);
        }
        Ok(self
            .black_frames(entry, Mode::Recap, (0.0, boundary), config)
            .await
            .inspect_err(|err| tracing::debug!(%err, item = entry.name, "intro skipper: recap black frames failed"))
            .ok()
            .and_then(|frames| {
                recap_from_black_frames(
                    &frames,
                    f64::from(config.minimum_recap_detection_duration),
                    boundary,
                )
            }))
    }

    /// The Chromaprint analyzer's recap step: both sides' cards become
    /// recaps, or the comparison is passed over (either missing, or longer
    /// than `maximum`).
    pub(super) async fn recap_pair(
        &self,
        (current, remaining): (&QueuedEpisode, &QueuedEpisode),
        (lhs, rhs): ((f64, f64), (f64, f64)),
        (maximum, config): (f64, &IntroSkipperConfig),
        frames: &mut HashMap<Uuid, Vec<BlackFrame>>,
    ) -> Result<Option<((f64, f64), (f64, f64))>, ServiceError> {
        let current = self.recap_from_card(current, lhs, config, frames).await?;
        let remaining = self.recap_from_card(remaining, rhs, config, frames).await?;
        Ok(match (current, remaining) {
            (Some(l), Some(r)) if l.1 - l.0 <= maximum && r.1 - r.0 <= maximum => Some((l, r)),
            _ => None,
        })
    }

    /// `BuildRecapFromChromaprintCandidateAsync`: a shared recap card (`card`)
    /// becomes a recap from the start to the last black frame after it,
    /// before the boundary. `frames` caches each item's black frames for the
    /// pass.
    pub(super) async fn recap_from_card(
        &self,
        entry: &QueuedEpisode,
        card: (f64, f64),
        config: &IntroSkipperConfig,
        frames: &mut HashMap<Uuid, Vec<BlackFrame>>,
    ) -> Result<Option<(f64, f64)>, ServiceError> {
        if card.1 <= 0.0 {
            return Ok(None);
        }
        let boundary = self.recap_boundary(entry, config).await?;
        if boundary <= card.1 {
            return Ok(None);
        }
        let frames = match frames.entry(entry.episode_id) {
            Entry::Occupied(cached) => cached.into_mut(),
            Entry::Vacant(slot) => slot.insert(
                self.black_frames(entry, Mode::Recap, (0.0, boundary), config)
                    .await
                    .inspect_err(|err| {
                        tracing::debug!(%err, item = entry.name, "intro skipper: recap black frames failed");
                    })
                    .unwrap_or_default(),
            ),
        };
        // `Math.Max(MinimumRecapDetectionDuration, (int)Math.Ceiling(card.End))`.
        let minimum = f64::from(config.minimum_recap_detection_duration).max(card.1.ceil());
        Ok(recap_from_black_frames(frames, minimum, boundary))
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // exact frame times
mod tests {
    use super::*;

    fn frame(percentage: i32, time: f64, frame: i64) -> BlackFrame {
        BlackFrame {
            percentage,
            time,
            frame,
        }
    }

    /// `TestChapterAnalyzer.BuildRecapFromBlackFrames_*`.
    #[rstest::rstest]
    // ReturnsSegmentFromStartToLatestFrameInRange
    #[case(&[frame(95, 32.5, 123), frame(92, 18.25, 90), frame(90, 45.0, 150)], 120.0, Some((0.0, 45.0)))]
    // ReturnsLatestFrameBeforeIntroBoundary
    #[case(&[frame(95, 32.5, 123), frame(92, 72.0, 250), frame(90, 90.0, 300)], 80.0, Some((0.0, 72.0)))]
    // ReturnsNull_WhenFrameBeforeMinimumDuration
    #[case(&[frame(90, 3.5, 20)], 120.0, None)]
    fn recaps_from_black_frames(
        #[case] frames: &[BlackFrame],
        #[case] maximum: f64,
        #[case] expected: Option<(f64, f64)>,
    ) {
        assert_eq!(recap_from_black_frames(frames, 5.0, maximum), expected);
    }
}
