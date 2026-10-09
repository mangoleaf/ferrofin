//! The chapter analyzer — port of the plugin's `ChapterAnalyzer`
//! (intro-skipper `db09359`): a segment is a chapter whose name matches the
//! mode's pattern (or SponsorBlock label), whose length is within the mode's
//! bounds, and whose neighbour (the next one; the previous one for credits and
//! previews, which are searched from the end) does not match too.

use std::collections::HashSet;
use std::sync::Mutex;

use fancy_regex::{Regex, RegexBuilder};
use ferrofin_model::entities_media::ChapterInfo;
use ferrofin_model::intro_skipper::AnalysisMode as Mode;
use ferrofin_traits::error::ServiceError;

use super::engine::Item;
use super::queue::{Category, QueuedEpisode};
use super::{DetectSegmentsTask, IntroSkipperConfig};

/// SponsorBlock labels that are ambiguous for any one mode: Commercial's.
const AMBIGUOUS_SPONSOR_BLOCK_LABELS: [&str; 5] = [
    "intermission/intro animation",
    "preview/recap",
    "preview/recap/hook",
    "hook",
    "hook/greetings",
];

/// `_sponsorBlockChapterLabels`: the SponsorBlock labels of each mode.
fn sponsor_block_labels(mode: Mode) -> &'static [&'static str] {
    match mode {
        Mode::Introduction => &["intro"],
        Mode::Credits => &["outro", "endcards/credits"],
        Mode::Preview => &["preview"],
        Mode::Recap => &["recap"],
        Mode::Commercial => &[
            "sponsor",
            "selfpromo",
            "self promotion",
            "unpaid/self promotion",
            "interaction",
            "interaction reminder",
            "interaction reminder (subscribe)",
            "intermission",
            "filler",
            "tangents/jokes",
            "music_offtopic",
            "music: non-music section",
            "non-music section",
        ],
        Mode::Unrecognized(_) => &[],
    }
}

/// The mode's chapter-name pattern.
fn pattern(c: &IntroSkipperConfig, mode: Mode) -> &str {
    match mode {
        Mode::Introduction => &c.chapter_analyzer_introduction_pattern,
        Mode::Credits => &c.chapter_analyzer_end_credits_pattern,
        Mode::Recap => &c.chapter_analyzer_recap_pattern,
        Mode::Preview => &c.chapter_analyzer_preview_pattern,
        Mode::Commercial => &c.chapter_analyzer_commercial_pattern,
        Mode::Unrecognized(_) => "",
    }
}

/// The mode's pattern compiled as .NET's `RegexOptions.IgnoreCase`; `None`
/// when blank. An invalid one matches nothing, reported once per pattern.
/// Accepted divergence: upstream's `Regex.IsMatch` throws and fails the whole
/// pass, whereas here the other analyzers and SponsorBlock labels still run,
/// and the pattern is part of the configuration hash, so fixing it
/// re-analyses.
fn compile(expression: &str) -> Option<Regex> {
    static REPORTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);
    if expression.trim().is_empty() {
        return None;
    }
    match RegexBuilder::new(expression).case_insensitive(true).build() {
        Ok(regex) => Some(regex),
        Err(err) => {
            let first = REPORTED.lock().map_or(true, |mut reported| {
                reported
                    .get_or_insert_default()
                    .insert(expression.to_owned())
            });
            if first {
                tracing::error!(%err, expression, "intro skipper: invalid chapter pattern; it matches nothing");
            }
            None
        }
    }
}

/// `TryGetSponsorBlockChapterLabel`: the label after `[SponsorBlock]:`.
fn sponsor_block_label(name: &str) -> Option<&str> {
    const PREFIX: &str = "[SponsorBlock]:";
    let head = name.get(..PREFIX.len())?;
    head.eq_ignore_ascii_case(PREFIX)
        .then(|| name[PREFIX.len()..].trim())
}

/// `ChapterMatches`: a SponsorBlock label of the mode, else the pattern. A
/// match that exhausts fancy-regex's backtracking budget is no match — the
/// same accepted divergence as an invalid pattern (upstream's 1 s
/// `RegexMatchTimeoutException` fails the pass).
fn chapter_matches(name: &str, regex: Option<&Regex>, mode: Mode, sponsor_block: bool) -> bool {
    if sponsor_block && let Some(label) = sponsor_block_label(name) {
        let ambiguous = mode == Mode::Commercial
            && AMBIGUOUS_SPONSOR_BLOCK_LABELS
                .iter()
                .any(|l| l.eq_ignore_ascii_case(label));
        if ambiguous
            || sponsor_block_labels(mode)
                .iter()
                .any(|l| l.eq_ignore_ascii_case(label))
        {
            return true;
        }
    }
    regex.is_some_and(|r| {
        r.is_match(name).unwrap_or_else(|err| {
            tracing::debug!(%err, name, "intro skipper: chapter pattern gave up; no match");
            false
        })
    })
}

/// `GetBounds`: the chapter lengths the mode accepts.
fn bounds(c: &IntroSkipperConfig, mode: Mode, entry: &QueuedEpisode) -> (f64, f64) {
    if c.full_length_chapters {
        // A second's margin at either end.
        return (1.0, entry.duration - 1.0);
    }
    let (min, max) = match mode {
        Mode::Introduction => (c.minimum_intro_duration, c.maximum_intro_duration),
        Mode::Credits => (
            c.minimum_credits_duration,
            if entry.category == Category::Movie {
                c.maximum_movie_credits_duration
            } else {
                c.maximum_credits_duration
            },
        ),
        Mode::Recap => (c.minimum_recap_duration, c.maximum_recap_duration),
        Mode::Preview => (c.minimum_preview_duration, c.maximum_preview_duration),
        Mode::Commercial | Mode::Unrecognized(_) => {
            (c.minimum_commercial_duration, c.maximum_commercial_duration)
        }
    };
    (f64::from(min), f64::from(max))
}

/// Ticks as seconds.
#[allow(clippy::cast_precision_loss)]
pub(super) fn seconds(ticks: i64) -> f64 {
    ticks as f64 / 10_000_000.0
}

/// `FindMatchingChapter`: the first acceptable chapter, searched from the end
/// for credits and previews. The last chapter runs to the end of the item.
fn find_matching_chapter(
    entry: &QueuedEpisode,
    chapters: &[ChapterInfo],
    regex: Option<&Regex>,
    mode: Mode,
    (c, sponsor_block): (&IntroSkipperConfig, bool),
) -> Option<(f64, f64)> {
    let reversed = matches!(mode, Mode::Credits | Mode::Preview);
    let (min, max) = bounds(c, mode, entry);
    let named = |i: usize| {
        chapters
            .get(i)
            .and_then(|ch| ch.name.as_deref())
            .filter(|n| !n.trim().is_empty())
    };
    let order: Box<dyn Iterator<Item = usize>> = if reversed {
        Box::new((0..chapters.len()).rev())
    } else {
        Box::new(0..chapters.len())
    };
    for i in order {
        let Some(name) = named(i) else {
            continue;
        };
        let start = seconds(chapters[i].start_position_ticks);
        let end = chapters
            .get(i + 1)
            .map_or(entry.duration, |next| seconds(next.start_position_ticks));
        let length = end - start;
        if length < min || length > max {
            tracing::trace!(
                item = entry.name,
                name,
                start,
                end,
                "intro skipper: chapter ignored (invalid duration)"
            );
            continue;
        }
        if !chapter_matches(name, regex, mode, sponsor_block) {
            continue;
        }
        let adjacent = if reversed {
            i.checked_sub(1).and_then(named)
        } else {
            named(i + 1)
        };
        if adjacent.is_some_and(|n| chapter_matches(n, regex, mode, sponsor_block)) {
            tracing::trace!(
                item = entry.name,
                name,
                "intro skipper: chapter ignored (adjacent chapter also matches)"
            );
            continue;
        }
        return Some((start, end));
    }
    None
}

impl DetectSegmentsTask {
    /// `ChapterAnalyzer.AnalyzeMediaFiles`: each item still needing the mode
    /// gets its matching chapter, adjusted (without chapter snapping) and
    /// stored.
    pub(super) async fn chapter_analyzer(
        &self,
        items: &mut [Item],
        mode: Mode,
        config: &IntroSkipperConfig,
    ) -> Result<(), ServiceError> {
        let sponsor_block = config.enable_sponsor_block_chapter_detection;
        let expression = pattern(config, mode);
        // TODO(intro-skipper step 8): with `DetectRecapUsingBlackFrames`, a
        // recap without a matching chapter falls back to black frames
        // (`DetectRecapUsingBlackFramesAsync`), and that also keeps this
        // analyzer running when there is no pattern.
        if expression.trim().is_empty() && !sponsor_block {
            return Ok(());
        }
        let regex = compile(expression);
        for item in items.iter_mut().filter(|i| i.needs_analysis(mode)) {
            let chapters = self.item_chapters(item.entry.episode_id).await;
            let Some(range) = find_matching_chapter(
                &item.entry,
                &chapters,
                regex.as_ref(),
                mode,
                (config, sponsor_block),
            )
            .filter(|(_, end)| *end > 0.0) else {
                continue;
            };
            // No chapter snapping: the bounds are chapters already.
            self.store_found(item, mode, range, config, false).await?;
        }
        Ok(())
    }
}

/// `TestChapterAnalyzer` (intro-skipper `db09359`).
#[cfg(test)]
#[allow(clippy::float_cmp)] // whole seconds
mod tests {
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;

    /// `FindChapter`: "Cold Open" at 0, the early chapter at 60, "Main
    /// Episode" at 90 and the late chapter at 1890, in a 2000 s episode; the
    /// name under test is the early chapter for introductions, recaps and
    /// commercials, the late one for credits and previews.
    fn find_chapter(
        name: &str,
        mode: Mode,
        expression: Option<&str>,
        sponsor_block: bool,
    ) -> Option<(f64, f64)> {
        let early = matches!(mode, Mode::Introduction | Mode::Recap | Mode::Commercial);
        let chapter = |name: &str, seconds: i64| ChapterInfo {
            start_position_ticks: seconds * 10_000_000,
            name: Some(name.to_owned()),
            image_path: None,
            image_date_modified: Utc::now(),
            image_tag: None,
        };
        let chapters = [
            chapter("Cold Open", 0),
            chapter(if early { name } else { "Introduction" }, 60),
            chapter("Main Episode", 90),
            chapter(if early { "Credits" } else { name }, 1890),
        ];
        let entry = QueuedEpisode {
            series_name: String::new(),
            season_number: 1,
            series_id: Uuid::nil(),
            season_id: Uuid::nil(),
            episode_number: 1,
            episode_id: Uuid::nil(),
            name: String::new(),
            category: Category::Episode,
            is_excluded: false,
            path: String::new(),
            duration: 2000.0,
            date_added: None,
            intro_fingerprint_end: 0.0,
            credits_fingerprint_start: 0.0,
            credits_fingerprint_end: 0.0,
        };
        let c = IntroSkipperConfig::default();
        let regex = compile(expression.unwrap_or_else(|| pattern(&c, mode)));
        find_matching_chapter(&entry, &chapters, regex.as_ref(), mode, (&c, sponsor_block))
    }

    #[rstest::rstest]
    // TestIntroductionExpression
    #[case("Opening", Mode::Introduction, Some((60.0, 90.0)))]
    #[case("OP", Mode::Introduction, Some((60.0, 90.0)))]
    #[case("Intro", Mode::Introduction, Some((60.0, 90.0)))]
    #[case("Intro:", Mode::Introduction, Some((60.0, 90.0)))]
    #[case("Intro Start", Mode::Introduction, Some((60.0, 90.0)))]
    #[case("Introduction", Mode::Introduction, Some((60.0, 90.0)))]
    // TestEndCreditsExpression
    #[case("End Credits", Mode::Credits, Some((1890.0, 2000.0)))]
    #[case("Ending", Mode::Credits, Some((1890.0, 2000.0)))]
    #[case("Credit start", Mode::Credits, Some((1890.0, 2000.0)))]
    #[case("Closing Credits", Mode::Credits, Some((1890.0, 2000.0)))]
    #[case("Credits", Mode::Credits, Some((1890.0, 2000.0)))]
    #[case("Credits:", Mode::Credits, Some((1890.0, 2000.0)))]
    // TestPreviewExpression
    #[case("Preview", Mode::Preview, Some((1890.0, 2000.0)))]
    #[case("Trailer", Mode::Preview, Some((1890.0, 2000.0)))]
    // TestRecapExpression
    #[case("Recap", Mode::Recap, Some((60.0, 90.0)))]
    #[case("Previously", Mode::Recap, Some((60.0, 90.0)))]
    // TestCommercialExpression
    #[case("Ad", Mode::Commercial, Some((60.0, 90.0)))]
    #[case("Advertisement", Mode::Commercial, Some((60.0, 90.0)))]
    #[case("Commercial", Mode::Commercial, Some((60.0, 90.0)))]
    #[case("Intermission", Mode::Commercial, Some((60.0, 90.0)))]
    // TestCommercialExpressionIgnoresSponsorBlockOnlyAndEndLabels
    #[case("Intermission/Intro", Mode::Commercial, None)]
    #[case("Intermission/Intro Animation", Mode::Commercial, None)]
    #[case("Commercial End", Mode::Commercial, None)]
    #[case("Intermission End", Mode::Commercial, None)]
    #[case("Intermission/Intro End", Mode::Commercial, None)]
    #[case("Intermission/Intro Animation End", Mode::Commercial, None)]
    // TestChapterExpressionIgnoresColonDelimitedEndLabels
    #[case("Intro: End", Mode::Introduction, None)]
    #[case("Credits: End", Mode::Credits, None)]
    #[case("Preview: End", Mode::Preview, None)]
    #[case("Recap: End", Mode::Recap, None)]
    #[case("Commercial: End", Mode::Commercial, None)]
    #[case("Intermission: End", Mode::Commercial, None)]
    // `RegexOptions.IgnoreCase`.
    #[case("opening", Mode::Introduction, Some((60.0, 90.0)))]
    #[case("INTRO: end", Mode::Introduction, None)]
    #[case("Intro: END", Mode::Introduction, None)]
    fn chapter_expressions(
        #[case] name: &str,
        #[case] mode: Mode,
        #[case] expected: Option<(f64, f64)>,
    ) {
        assert_eq!(find_chapter(name, mode, None, true), expected);
    }

    /// `TestSponsorBlockChapterLabelsMapToExpectedMode`: with no pattern, a
    /// SponsorBlock label matches its mode and no other.
    #[rstest::rstest]
    #[case("[SponsorBlock]: Intro", Mode::Introduction)]
    #[case("[SponsorBlock]: intro", Mode::Introduction)]
    #[case("[SponsorBlock]: Endcards/Credits", Mode::Credits)]
    #[case("[SponsorBlock]: Outro", Mode::Credits)]
    #[case("[SponsorBlock]: outro", Mode::Credits)]
    #[case("[SponsorBlock]: Preview", Mode::Preview)]
    #[case("[SponsorBlock]: preview", Mode::Preview)]
    #[case("[SponsorBlock]: Recap", Mode::Recap)]
    #[case("[SponsorBlock]: Sponsor", Mode::Commercial)]
    #[case("[SponsorBlock]: sponsor", Mode::Commercial)]
    #[case("[SponsorBlock]: Unpaid/Self Promotion", Mode::Commercial)]
    #[case("[SponsorBlock]: Self Promotion", Mode::Commercial)]
    #[case("[SponsorBlock]: selfpromo", Mode::Commercial)]
    #[case("[SponsorBlock]: Interaction Reminder (Subscribe)", Mode::Commercial)]
    #[case("[SponsorBlock]: interaction", Mode::Commercial)]
    #[case("[SponsorBlock]: Tangents/Jokes", Mode::Commercial)]
    #[case("[SponsorBlock]: Filler", Mode::Commercial)]
    #[case("[SponsorBlock]: filler", Mode::Commercial)]
    #[case("[SponsorBlock]: Music: Non-Music Section", Mode::Commercial)]
    #[case("[SponsorBlock]: Non-Music Section", Mode::Commercial)]
    #[case("[SponsorBlock]: music_offtopic", Mode::Commercial)]
    #[case("[SponsorBlock]: Intermission", Mode::Commercial)]
    #[case("[SponsorBlock]: Intermission/Intro Animation", Mode::Commercial)]
    #[case("[SponsorBlock]: Preview/Recap", Mode::Commercial)]
    #[case("[SponsorBlock]: Preview/Recap/Hook", Mode::Commercial)]
    #[case("[SponsorBlock]: Hook/Greetings", Mode::Commercial)]
    #[case("[SponsorBlock]: hook", Mode::Commercial)]
    fn sponsor_block_labels_map_to_their_mode(#[case] name: &str, #[case] expected: Mode) {
        for mode in [
            Mode::Introduction,
            Mode::Credits,
            Mode::Recap,
            Mode::Preview,
            Mode::Commercial,
        ] {
            let found = find_chapter(name, mode, Some(""), true);
            assert_eq!(found.is_some(), mode == expected, "{name} as {mode:?}");
        }
    }

    #[test]
    fn sponsor_block_detection_can_be_disabled() {
        let name = "[SponsorBlock]: Intermission/Intro Animation";
        assert_eq!(find_chapter(name, Mode::Commercial, Some(""), false), None);
    }

    #[test]
    fn an_unmapped_sponsor_block_chapter_falls_back_to_the_pattern() {
        let found = find_chapter(
            "[SponsorBlock]: Custom Intro",
            Mode::Introduction,
            Some("Custom Intro"),
            true,
        );
        assert_eq!(found, Some((60.0, 90.0)));
    }

    /// An invalid pattern matches nothing (SponsorBlock labels still do).
    #[test]
    fn an_invalid_pattern_matches_nothing() {
        assert_eq!(
            find_chapter("Intro", Mode::Introduction, Some("(unclosed"), true),
            None
        );
        assert_eq!(
            find_chapter(
                "[SponsorBlock]: Intro",
                Mode::Introduction,
                Some("(unclosed"),
                true
            ),
            Some((60.0, 90.0))
        );
    }

    /// `GetBounds`: a chapter outside the mode's lengths is ignored, unless
    /// `FullLengthChapters`.
    #[test]
    fn chapter_lengths_follow_the_bounds() {
        let entry = |category| QueuedEpisode {
            series_name: String::new(),
            season_number: 1,
            series_id: Uuid::nil(),
            season_id: Uuid::nil(),
            episode_number: 1,
            episode_id: Uuid::nil(),
            name: String::new(),
            category,
            is_excluded: false,
            path: String::new(),
            duration: 7200.0,
            date_added: None,
            intro_fingerprint_end: 0.0,
            credits_fingerprint_start: 0.0,
            credits_fingerprint_end: 0.0,
        };
        let c = IntroSkipperConfig::default();
        assert_eq!(
            bounds(&c, Mode::Credits, &entry(Category::Episode)),
            (15.0, 450.0)
        );
        assert_eq!(
            bounds(&c, Mode::Credits, &entry(Category::Movie)),
            (15.0, 900.0)
        );
        let full = IntroSkipperConfig {
            full_length_chapters: true,
            ..c
        };
        assert_eq!(
            bounds(&full, Mode::Preview, &entry(Category::Episode)),
            (1.0, 7199.0)
        );
    }
}
