//! The analysis configuration hash — port of the plugin's
//! `ConfigHasher.Analysis` (intro-skipper `db09359`).
//!
//! Every setting that changes a mode's result goes into a fixed string; its
//! hash is stored with each analysed segment and with the season state, so a
//! settings change re-analyses exactly what it affects. The string follows
//! upstream's (C# `FormattableString.Invariant`: `True`/`False`, shortest
//! round-trip numbers); the hash is the first 8 bytes of its SHA-256, upper-
//! case hex.

use std::fmt::Write as _;

use ferrofin_model::intro_skipper::{AnalysisMode, AnalyzerAction};
use sha2::{Digest, Sha256};

use super::IntroSkipperConfig;

/// Which analyzers and adjustments this build runs, folded into every hash.
/// Bump it when one lands — the chapter and black-frame analyzers, the recap
/// path (steps 7–8), and the silence/keyframe snapping (step 6, which the
/// stored `silence=`/`keyframe=` settings already claim) — so what earlier
/// passes recorded is analysed again with it, rather than stranded under an
/// unchanged hash. Ferrofin's hashes are its own (the plugin's database is
/// not imported), so the token costs no compatibility.
const ANALYZERS: &str = "chromaprint";

/// C#'s `bool.ToString()`.
fn b(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

/// The mode's name as C# writes the enum (`Unrecognized` as its number).
fn mode_name(mode: AnalysisMode) -> String {
    match mode {
        AnalysisMode::Introduction => "Introduction".to_owned(),
        AnalysisMode::Credits => "Credits".to_owned(),
        AnalysisMode::Preview => "Preview".to_owned(),
        AnalysisMode::Recap => "Recap".to_owned(),
        AnalysisMode::Commercial => "Commercial".to_owned(),
        AnalysisMode::Unrecognized(value) => value.to_string(),
    }
}

/// The action's name as C# writes the enum.
fn action_name(action: AnalyzerAction) -> String {
    match action {
        AnalyzerAction::Default => "Default".to_owned(),
        AnalyzerAction::Chapter => "Chapter".to_owned(),
        AnalyzerAction::Chromaprint => "Chromaprint".to_owned(),
        AnalyzerAction::BlackFrame => "BlackFrame".to_owned(),
        AnalyzerAction::None => "None".to_owned(),
        AnalyzerAction::Unrecognized(value) => value.to_string(),
    }
}

/// `ConfigHasher.Analysis(config, mode, action, ffmpegValid)`.
#[must_use]
pub(super) fn analysis(
    c: &IntroSkipperConfig,
    mode: AnalysisMode,
    action: AnalyzerAction,
    ffmpeg_valid: bool,
) -> String {
    let (m, a) = (mode_name(mode), action_name(action));
    let fingerprint = format!(
        "|fpbits={}|skip={}|shift={}|chromaprint={}",
        c.maximum_fingerprint_point_differences,
        c.maximum_time_skip,
        c.inverted_index_shift,
        b(ffmpeg_valid)
    );
    let input = match mode {
        AnalysisMode::Introduction => format!(
            "analysis|v1|mode={m}|action={a}|prefer={}|chap={}|fullchap={}|sbchap={}\
             |pct={}|limit={}|min={}|max={}{fingerprint}{}",
            b(c.prefer_chromaprint),
            c.chapter_analyzer_introduction_pattern,
            b(c.full_length_chapters),
            b(c.enable_sponsor_block_chapter_detection),
            c.analysis_percent,
            c.analysis_length_limit,
            c.minimum_intro_duration,
            c.maximum_intro_duration,
            adjustment(c),
        ),
        AnalysisMode::Credits => format!(
            "analysis|v1|mode={m}|action={a}|prefer={}|chap={}|fullchap={}|sbchap={}\
             |pct={}|maxCredits={}|maxMovie={}|probe={}\
             |min={}|bfmin={}|bfthr={}|bfchap={}\
             |bfalt={}|bfrefine={}|bfaltVersion=2{}{fingerprint}\
             |animePreview={}{}",
            b(c.prefer_chromaprint),
            c.chapter_analyzer_end_credits_pattern,
            b(c.full_length_chapters),
            b(c.enable_sponsor_block_chapter_detection),
            c.analysis_percent,
            c.maximum_credits_duration,
            c.maximum_movie_credits_duration,
            b(c.probe_audio_duration),
            c.minimum_credits_duration,
            c.black_frame_minimum_percentage,
            c.black_frame_threshold,
            b(c.use_chapter_markers_black_frame),
            b(c.use_alternative_black_frame_analyzer),
            b(c.refine_credits_boundary),
            // Only the alternative analyzer reads DetectNonBlackCredits.
            if c.use_alternative_black_frame_analyzer {
                format!("|nonblack={}", b(c.detect_non_black_credits))
            } else {
                String::new()
            },
            b(c.anime_preview_from_credits_end),
            adjustment(c),
        ),
        AnalysisMode::Recap => format!(
            "analysis|v1|mode={m}|action={a}|prefer={}|chap={}|fullchap={}|sbchap={}|min={}|max={}\
             |detMin={}|detMax={}\
             |recapBlackFrames={}|bfmin={}|bfthr={}\
             |pct={}|limit={}{fingerprint}{}",
            b(c.prefer_chromaprint),
            c.chapter_analyzer_recap_pattern,
            b(c.full_length_chapters),
            b(c.enable_sponsor_block_chapter_detection),
            c.minimum_recap_duration,
            c.maximum_recap_duration,
            c.minimum_recap_detection_duration,
            c.maximum_recap_detection_duration,
            b(c.detect_recap_using_black_frames),
            c.black_frame_minimum_percentage,
            c.black_frame_threshold,
            c.analysis_percent,
            c.analysis_length_limit,
            adjustment(c),
        ),
        AnalysisMode::Preview => format!(
            "analysis|v1|mode={m}|action={a}|chap={}|fullchap={}|sbchap={}|min={}|max={}{}",
            c.chapter_analyzer_preview_pattern,
            b(c.full_length_chapters),
            b(c.enable_sponsor_block_chapter_detection),
            c.minimum_preview_duration,
            c.maximum_preview_duration,
            adjustment(c),
        ),
        AnalysisMode::Commercial | AnalysisMode::Unrecognized(_) => format!(
            "analysis|v1|mode={m}|action={a}|chap={}|fullchap={}|sbchap={}|min={}|max={}{}",
            c.chapter_analyzer_commercial_pattern,
            b(c.full_length_chapters),
            b(c.enable_sponsor_block_chapter_detection),
            c.minimum_commercial_duration,
            c.maximum_commercial_duration,
            adjustment(c),
        ),
    };
    hash(&format!("{input}|ferrofin-analyzers={ANALYZERS}"))
}

/// `ConfigHasher.AdjustmentHash`: the time-adjustment settings every mode
/// shares.
fn adjustment(c: &IntroSkipperConfig) -> String {
    format!(
        "|chapAdjust={}|silence={}|keyframe={}\
         |endSnap={}|winIn={}|winOut={}\
         |noise={}|silDur={}\
         |startOffset={}|endOffset={}",
        b(c.adjust_intro_based_on_chapters),
        b(c.adjust_intro_based_on_silence),
        b(c.snap_to_keyframe),
        c.end_snap_threshold,
        c.adjust_window_inward,
        c.adjust_window_outward,
        c.silence_detection_maximum_noise,
        c.silence_detection_minimum_duration,
        c.intro_start_offset,
        c.intro_end_offset,
    )
}

/// `Convert.ToHexString(SHA256(input), 0, 8)`.
fn hash(input: &str) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let mut out = String::with_capacity(16);
    for byte in &digest[..8] {
        let _ = write!(out, "{byte:02X}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_is_eight_upper_hex_bytes_of_sha256() {
        // SHA-256("abc") = BA7816BF8F01CFEA…
        assert_eq!(hash("abc"), "BA7816BF8F01CFEA");
    }

    #[test]
    fn a_setting_changes_only_the_modes_that_read_it() {
        let base = IntroSkipperConfig::default();
        let tweak = IntroSkipperConfig {
            maximum_credits_duration: base.maximum_credits_duration + 1,
            ..IntroSkipperConfig::default()
        };
        let intro = |c| analysis(c, AnalysisMode::Introduction, AnalyzerAction::Default, true);
        let credits = |c| analysis(c, AnalysisMode::Credits, AnalyzerAction::Default, true);
        assert_eq!(intro(&base), intro(&tweak));
        assert_ne!(credits(&base), credits(&tweak));
        // The action and the Chromaprint availability are part of it too.
        assert_ne!(
            intro(&base),
            analysis(
                &base,
                AnalysisMode::Introduction,
                AnalyzerAction::Chapter,
                true
            )
        );
        assert_ne!(
            intro(&base),
            analysis(
                &base,
                AnalysisMode::Introduction,
                AnalyzerAction::Default,
                false
            )
        );
        assert_eq!(intro(&base).len(), 16);
    }

    /// `AnalysisHash_ChangesWithChromaprintAvailability_ForChromaprintModes`
    /// and `AnalysisHash_IgnoresChromaprintAvailability_ForChapterOnlyModes`.
    #[rstest::rstest]
    #[case(AnalysisMode::Introduction, true)]
    #[case(AnalysisMode::Credits, true)]
    #[case(AnalysisMode::Recap, true)]
    #[case(AnalysisMode::Preview, false)]
    #[case(AnalysisMode::Commercial, false)]
    fn chromaprint_availability_changes_only_chromaprint_modes(
        #[case] mode: AnalysisMode,
        #[case] changes: bool,
    ) {
        let c = IntroSkipperConfig::default();
        let with = analysis(&c, mode, AnalyzerAction::Default, true);
        let without = analysis(&c, mode, AnalyzerAction::Default, false);
        assert_eq!(with != without, changes);
    }
}
