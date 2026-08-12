//! Pure Chromaprint fingerprint comparison for intro/credits detection.
//!
//! A faithful port of the Intro Skipper plugin's comparison math
//! (`IntroSkipper/Analyzers/ChromaprintAnalyzer.cs` +
//! `Data/TimeRangeHelpers.cs`, GPL-3.0-only, github.com/intro-skipper/intro-skipper).
//! Given two episodes' Chromaprint fingerprints (arrays of `u32` points, one per
//! ~0.124 s of audio), it finds the shared region — the intro or the credits.
//!
//! This crate is pure: no I/O, no ffmpeg. Producing the fingerprints (via
//! `fpcalc`/ffmpeg) and turning the result into media segments lives in the
//! `ferrofin-extensions` intro-skipper module; keeping the math here makes it
//! unit-testable against the C# constants as the oracle.
//!
//! ## Algorithm
//!
//! 1. Build an inverted index (`point → last index`) for each fingerprint.
//! 2. For every left point, look for a right point within `±inverted_index_shift`
//!    of its value; each hit yields a candidate index *shift* (`rhs_i − lhs_i`).
//! 3. For each candidate shift, XOR the two fingerprints at that alignment and
//!    keep the positions whose bitwise difference is `≤ max_bit_diff` bits.
//! 4. The longest run of kept positions whose gaps are `≤ max_time_skip` seconds
//!    is that shift's shared region.
//! 5. Across all shifts, select the longest region (intro/credits) or the
//!    earliest (recap); snap a start within 5 s to 0.

use std::collections::HashMap;

/// Seconds of audio each Chromaprint point represents.
///
/// Fixed by Chromaprint: sample rate 11025 Hz, frame size 4096, 2/3 overlap
/// (hop = frame/3). `4096 / 11025 / 3 ≈ 0.12383 s`.
pub const SAMPLE_DURATION: f64 = 4096.0 / 11025.0 / 3.0;

/// Which kind of shared region is being detected (drives region selection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisMode {
    /// An episode's opening titles — the longest shared region.
    Introduction,
    /// End credits — the longest shared region (of the tail audio).
    Credits,
    /// A "previously on" recap — the earliest shared region.
    Recap,
}

/// A half-open time span in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TimeRange {
    /// Start time (seconds).
    pub start: f64,
    /// End time (seconds).
    pub end: f64,
}

impl TimeRange {
    /// A range covering `[start, end]`.
    #[must_use]
    pub fn new(start: f64, end: f64) -> Self {
        Self { start, end }
    }

    /// The span in seconds (`end − start`).
    #[must_use]
    pub fn duration(&self) -> f64 {
        self.end - self.start
    }
}

/// The tunable comparison parameters (Intro Skipper's `PluginConfiguration`
/// analysis knobs). Defaults mirror the C# defaults.
#[derive(Debug, Clone, Copy)]
pub struct CompareConfig {
    /// Fuzzy point-value tolerance when matching points across episodes
    /// (`InvertedIndexShift`, default 2).
    pub inverted_index_shift: i32,
    /// Maximum Hamming distance (set bits, out of 32) two points may differ and
    /// still be "similar" (`MaximumFingerprintPointDifferences`, default 6).
    pub max_bit_diff: u32,
    /// Maximum gap (seconds) between similar points before the run breaks
    /// (`MaximumTimeSkip`, default 3.5).
    pub max_time_skip: f64,
    /// Minimum duration (seconds) a shared region must span to count
    /// (`MinimumIntroDuration`, default 15; recap uses 3).
    pub min_region_duration: f64,
}

impl Default for CompareConfig {
    fn default() -> Self {
        Self {
            inverted_index_shift: 2,
            max_bit_diff: 6,
            max_time_skip: 3.5,
            min_region_duration: 15.0,
        }
    }
}

/// The start time (seconds) of the fingerprint point at index `i`.
///
/// A fingerprint has far fewer than 2^52 points, so the `usize → f64` conversion
/// is exact in practice.
#[allow(clippy::cast_precision_loss)]
fn point_time(i: usize) -> f64 {
    i as f64 * SAMPLE_DURATION
}

/// Builds the inverted index `point value → last index it appears at`.
///
/// Port of `CreateInvertedIndex`.
#[must_use]
pub fn create_inverted_index(fingerprint: &[u32]) -> HashMap<u32, usize> {
    let mut index = HashMap::with_capacity(fingerprint.len());
    for (i, &point) in fingerprint.iter().enumerate() {
        index.insert(point, i);
    }
    index
}

/// Finds every shared time-range candidate between two fingerprints, as parallel
/// `(lhs, rhs)` range lists (one pair per productive index shift).
///
/// Port of `SearchInvertedIndex`.
#[must_use]
pub fn search_inverted_index(
    lhs: &[u32],
    rhs: &[u32],
    cfg: &CompareConfig,
) -> (Vec<TimeRange>, Vec<TimeRange>) {
    let lhs_index = create_inverted_index(lhs);
    let rhs_index = create_inverted_index(rhs);

    // Candidate index shifts: for each left point, if the right episode has a
    // point within ±inverted_index_shift of its value, the two align at
    // `rhs_i − lhs_i`.
    let mut shifts = std::collections::HashSet::new();
    for (&point, &lhs_i) in &lhs_index {
        for delta in -cfg.inverted_index_shift..=cfg.inverted_index_shift {
            let modified = point.wrapping_add_signed(delta);
            if let Some(&rhs_i) = rhs_index.get(&modified) {
                // Through i64 so a large index difference can't overflow / wrap.
                let shift = i64::try_from(rhs_i).unwrap_or(i64::MAX)
                    - i64::try_from(lhs_i).unwrap_or(i64::MAX);
                shifts.insert(shift);
            }
        }
    }

    let mut lhs_ranges = Vec::new();
    let mut rhs_ranges = Vec::new();
    for shift in shifts {
        if let Some((l, r)) = find_contiguous_at_shift(lhs, rhs, shift, cfg) {
            lhs_ranges.push(l);
            rhs_ranges.push(r);
        }
    }
    (lhs_ranges, rhs_ranges)
}

/// XORs the two fingerprints at the given index `shift`, keeps positions whose
/// bitwise difference is within `max_bit_diff`, and returns the longest
/// contiguous run (if it meets `min_region_duration`).
///
/// Port of the private `FindContiguous(uint[], uint[], int)`.
fn find_contiguous_at_shift(
    lhs: &[u32],
    rhs: &[u32],
    shift: i64,
    cfg: &CompareConfig,
) -> Option<(TimeRange, TimeRange)> {
    // Align: a negative shift advances the left side, a positive one the right.
    let (left_offset, right_offset) = if shift < 0 {
        (usize::try_from(-shift).ok()?, 0)
    } else {
        (0, usize::try_from(shift).ok()?)
    };

    let abs_shift = usize::try_from(shift.unsigned_abs()).unwrap_or(usize::MAX);
    let upper = lhs.len().min(rhs.len()).saturating_sub(abs_shift);
    let mut lhs_times = Vec::new();
    let mut rhs_times = Vec::new();
    for i in 0..upper {
        let lp = i + left_offset;
        let rp = i + right_offset;
        // Bounds are guaranteed by `upper`, but stay defensive.
        let (Some(&lv), Some(&rv)) = (lhs.get(lp), rhs.get(rp)) else {
            break;
        };
        if (lv ^ rv).count_ones() > cfg.max_bit_diff {
            continue;
        }
        lhs_times.push(point_time(lp));
        rhs_times.push(point_time(rp));
    }

    let l = find_contiguous(&mut lhs_times, cfg.max_time_skip)?;
    if l.duration() < cfg.min_region_duration {
        return None;
    }
    // If LHS had a contiguous run, RHS (the same matched points) has one too.
    let r = find_contiguous(&mut rhs_times, cfg.max_time_skip)?;
    Some((l, r))
}

/// Returns the longest span of `times` whose successive gaps are within
/// `max_distance` seconds. `times` is sorted in place.
///
/// Port of `TimeRangeHelpers.FindContiguous`.
#[must_use]
pub fn find_contiguous(times: &mut [f64], max_distance: f64) -> Option<TimeRange> {
    if times.is_empty() {
        return None;
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let mut current = TimeRange::new(times[0], times[0]);
    let mut best = current;
    for window in times.windows(2) {
        let (cur, next) = (window[0], window[1]);
        if next - cur <= max_distance {
            current.end = next;
            continue;
        }
        if current.duration() > best.duration() {
            best = current;
        }
        current = TimeRange::new(next, next);
    }
    Some(if current.duration() > best.duration() {
        current
    } else {
        best
    })
}

/// Compares two episodes' fingerprints and returns the selected shared region for
/// each (`None` when no region is found).
///
/// Port of `CompareEpisodes` + `SelectSharedRegion`: the longest region for
/// intro/credits, the earliest for recap. A region starting within 5 s of 0 is
/// snapped to 0 (the C# start-of-episode adjustment).
#[must_use]
pub fn compare_episodes(
    lhs: &[u32],
    rhs: &[u32],
    mode: AnalysisMode,
    cfg: &CompareConfig,
) -> (Option<TimeRange>, Option<TimeRange>) {
    let (lhs_ranges, rhs_ranges) = search_inverted_index(lhs, rhs, cfg);
    if lhs_ranges.is_empty() {
        return (None, None);
    }
    let (mut l, mut r) = select_region(&lhs_ranges, &rhs_ranges, mode);
    snap_start(&mut l);
    snap_start(&mut r);
    (Some(l), Some(r))
}

/// Picks the winning `(lhs, rhs)` pair: the longest region for intro/credits, the
/// earliest for recap. The two lists are parallel (index = shift), so the same
/// index is chosen for both.
fn select_region(
    lhs: &[TimeRange],
    rhs: &[TimeRange],
    mode: AnalysisMode,
) -> (TimeRange, TimeRange) {
    let pick = (0..lhs.len().min(rhs.len())).max_by(|&a, &b| {
        match mode {
            // Earliest start wins for recap → reverse the start comparison.
            AnalysisMode::Recap => lhs[b].start.total_cmp(&lhs[a].start),
            // Longest duration wins otherwise.
            _ => lhs[a].duration().total_cmp(&lhs[b].duration()),
        }
    });
    let i = pick.unwrap_or(0);
    (lhs[i], rhs[i])
}

/// Snaps a region starting within 5 s of the episode start to exactly 0.
fn snap_start(range: &mut TimeRange) {
    if range.start <= 5.0 {
        range.start = 0.0;
    }
}

#[cfg(test)]
#[allow(clippy::float_cmp)] // snapping produces exactly 0.0; exact compare is intended
mod tests {
    use super::*;

    /// A fingerprint whose first `intro_len` points are a fixed pattern (the
    /// shared "intro"), followed by `body_len` points that differ per episode.
    fn synth(intro_len: u32, body_len: u32, seed: u32) -> Vec<u32> {
        let mut fp = Vec::with_capacity((intro_len + body_len) as usize);
        for i in 0..intro_len {
            fp.push(0xA5A5_0000 | i); // identical intro across episodes
        }
        for i in 0..body_len {
            fp.push(seed.wrapping_mul(2_654_435_761).wrapping_add(i));
        }
        fp
    }

    #[test]
    fn sample_duration_matches_chromaprint() {
        assert!((SAMPLE_DURATION - 0.123_839_758).abs() < 1e-6);
    }

    #[test]
    fn count_bits_is_popcount() {
        assert_eq!((0b0101u32 ^ 0b0110u32).count_ones(), 2);
        assert_eq!((0xFFFF_FFFFu32 ^ 0xFFFF_FFFFu32).count_ones(), 0);
    }

    #[test]
    fn inverted_index_keeps_last_occurrence() {
        let idx = create_inverted_index(&[7, 7, 9]);
        assert_eq!(idx[&7], 1); // last index of value 7
        assert_eq!(idx[&9], 2);
    }

    #[test]
    fn find_contiguous_returns_longest_run() {
        // Two runs: [0..0.5] (gap) [10..14]; the second is longer.
        let mut times = vec![0.0, 0.25, 0.5, 10.0, 11.0, 12.0, 13.0, 14.0];
        let r = find_contiguous(&mut times, 3.5).expect("range");
        assert!((r.start - 10.0).abs() < 1e-9);
        assert!((r.end - 14.0).abs() < 1e-9);
    }

    #[test]
    fn detects_a_shared_intro_at_the_same_offset() {
        // Both episodes share a ~200-point (~24.7 s) identical intro at index 0.
        let a = synth(200, 300, 1);
        let b = synth(200, 300, 2);
        let (la, lb) = compare_episodes(
            &a,
            &b,
            AnalysisMode::Introduction,
            &CompareConfig::default(),
        );
        let (la, lb) = (la.expect("lhs intro"), lb.expect("rhs intro"));
        // Starts snap to 0; the region spans roughly the intro length.
        assert_eq!(la.start, 0.0);
        assert_eq!(lb.start, 0.0);
        let expected_end = 200.0 * SAMPLE_DURATION;
        assert!(
            (la.end - expected_end).abs() < 1.0,
            "end ~ {expected_end}, got {}",
            la.end
        );
    }

    #[test]
    fn shared_intro_survives_a_time_shift_between_episodes() {
        // Episode B's intro starts 30 points later than A's — the inverted-index
        // shift search must still align them.
        let a = synth(200, 300, 1);
        let mut b = vec![0u32; 30];
        b.extend(synth(200, 300, 2));
        let (la, lb) = compare_episodes(
            &a,
            &b,
            AnalysisMode::Introduction,
            &CompareConfig::default(),
        );
        let la = la.expect("lhs intro");
        let lb = lb.expect("rhs intro");
        assert_eq!(la.start, 0.0); // A's intro at 0
        // B's intro starts 30 points in (~3.7 s) — within the snap threshold? 30 *
        // 0.1238 ≈ 3.7 s ≤ 5 → snapped to 0.
        assert_eq!(lb.start, 0.0);
        assert!(la.duration() > 15.0);
    }

    #[test]
    fn no_shared_region_when_episodes_differ() {
        let a = synth(0, 500, 1);
        let b = synth(0, 500, 2);
        let (la, _) = compare_episodes(
            &a,
            &b,
            AnalysisMode::Introduction,
            &CompareConfig::default(),
        );
        assert!(la.is_none());
    }

    #[test]
    fn too_short_a_shared_region_is_rejected() {
        // Only ~50 points (~6 s) shared — below the 15 s minimum.
        let a = synth(50, 500, 1);
        let b = synth(50, 500, 2);
        let (la, _) = compare_episodes(
            &a,
            &b,
            AnalysisMode::Introduction,
            &CompareConfig::default(),
        );
        assert!(la.is_none());
    }
}
