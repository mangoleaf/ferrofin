//! `GuardedRegex` must be a pure optimisation: for every vendored pattern and
//! every input, the guarded result has to equal the raw `fancy-regex` result.
//!
//! The guard works by deleting lookaround assertions to build a linear-time
//! superset matcher used only to reject. If that relaxation were ever *not* a
//! superset the guard would swallow real matches, so these tests diff the two
//! engines over a corpus that deliberately includes inputs which only the
//! lookarounds reject.

use ferrofin_naming::common::{GuardedRegex, NamingOptions};

/// Every pattern the crate runs through `GuardedRegex`, with its raw source.
fn vendored_patterns(options: &NamingOptions) -> Vec<String> {
    let mut out = Vec::new();
    for e in &options.episode_expressions {
        out.push(format!("(?i){}", e.expression()));
    }
    for e in &options.multiple_episode_expressions {
        out.push(format!("(?i){}", e.expression()));
    }
    for e in &options.clean_date_times {
        out.push(format!("(?i){e}"));
    }
    for e in &options.clean_strings {
        out.push(format!("(?i){e}"));
    }
    out
}

/// Inputs chosen to straddle the lookarounds: real library paths, plus the
/// near-misses that the assertions (and only the assertions) reject.
fn corpus() -> Vec<String> {
    let mut v: Vec<String> = [
        "",
        "/",
        ".mkv",
        "/media/tv/The Show/Season 1/The Show - S01E05 - Episode Title.mkv",
        "/media/tv/The Show/Season 12/The Show S12E24 1080p.mkv",
        "/media/tv/Another/Season 2/Another.2x07.HDTV.avi",
        "/media/tv/Daily/2019.03.14 - Something.mp4",
        "/media/tv/Daily/14.03.2019 - Something.mp4",
        "/media/tv/Anime/[Group] Anime Title [12].mkv",
        "/media/tv/Multi/Season 3/Show - S03E01-E02 - Two Parter.mkv",
        "/media/tv/Multi/Season 3/Show - 3x01x02.mkv",
        "/media/tv/Old/Season 4/04 - Some Title.avi",
        "/media/tv/Show/Specials/Show - S00E01 - Special.mkv",
        "/media/tv/Show/Season 1/Episode 5.mkv",
        "/media/tv/Show/Season 1/ep01.mkv",
        "/media/tv/Show/Season 1/E01.mkv",
        "/media/tv/Show/Season 1/1-12 episode title.mkv",
        "/media/tv/Show/Season 1/01.blah.avi",
        "/media/tv/Show/Season 1/blah - 01.avi",
        "/media/tv/Show/s01/01.mkv",
        "/media/tv/Show 2016/Season 1/Show 2016 S01E01.mkv",
        // clean_date_time: a bare year vs. a year followed by more digits (the
        // `(?![0-9]+|\W[0-9]{2}\W[0-9]{2})` assertion is the only difference).
        "The Movie (1999)",
        "The Movie 1999",
        "The Movie 19999",
        "The Movie 1999 12 31",
        "The Movie.1999.10.10",
        "The Movie_1999_1080p",
        "The Movie (2017) [1080p] [WEBRip] [5.1] [YTS.MX]",
        // clean_string: the `(?=[ _,.()\[\]-]|$)` trailing assertion.
        "The Movie 1080p",
        "The Movie 1080pxx",
        "The Movie.x264",
        "The Movie.x264extra",
        "The Movie hsbs 3d",
        "[Group] Title.mkv",
        "[Group] Title",
        "Movie - trailer",
        "Movie-behindthescenes",
        // Episode-expression assertions.
        "/media/tv/Show/Season 1/Episode 5 - 1080p.mkv",
        "/media/tv/Show/1920x1080/Show 1x01.mkv",
        "/media/tv/Show/Season 1/Show 1x01x02x03.mkv",
        "/media/tv/Show/Season 1/Show S01E01-1080p.mkv",
        "/media/tv/Show/Season 1/Show 100 101.mkv",
        "/media/tv/Show/Season 1/Show 1234.mkv",
        "C:\\media\\tv\\Show\\Season 1\\Show S01E01.mkv",
        "тв/Шоу/Сезон 1/Шоу S01E01.mkv",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();

    // Mechanically widen the corpus: separator and case variants exercise the
    // character classes inside the assertions.
    let base = v.clone();
    for s in &base {
        v.push(s.replace(' ', "."));
        v.push(s.replace(' ', "_"));
        v.push(s.to_uppercase());
        v.push(s.to_lowercase());
    }
    v
}

#[test]
fn guard_never_changes_the_match() {
    let options = NamingOptions::new();
    let inputs = corpus();
    let mut guarded_rejections = 0usize;
    let mut real_matches = 0usize;

    for pattern in vendored_patterns(&options) {
        let guarded = GuardedRegex::new(&pattern).expect("vendored pattern compiles");
        let raw = fancy_regex::Regex::new(&pattern).expect("vendored pattern compiles");

        for input in &inputs {
            let got = guarded.captures(input).expect("no backtrack blowup");
            let want = raw.captures(input).expect("no backtrack blowup");

            match (&got, &want) {
                (None, None) => guarded_rejections += 1,
                (Some(g), Some(w)) => {
                    real_matches += 1;
                    assert_eq!(
                        g.len(),
                        w.len(),
                        "group count differs for {pattern:?} on {input:?}"
                    );
                    for i in 0..w.len() {
                        assert_eq!(
                            g.get(i).map(|m| (m.start(), m.end())),
                            w.get(i).map(|m| (m.start(), m.end())),
                            "group {i} differs for {pattern:?} on {input:?}"
                        );
                    }
                }
                _ => panic!(
                    "guarded={} raw={} for {pattern:?} on {input:?}",
                    got.is_some(),
                    want.is_some()
                ),
            }

            assert_eq!(
                guarded.is_match(input).expect("no backtrack blowup"),
                raw.is_match(input).expect("no backtrack blowup"),
                "is_match differs for {pattern:?} on {input:?}"
            );
        }
    }

    // The corpus must actually exercise both outcomes, or the diff above is
    // vacuous.
    assert!(real_matches > 100, "corpus produced too few matches");
    assert!(
        guarded_rejections > 100,
        "corpus produced too few rejections"
    );
}

#[test]
fn every_vendored_pattern_actually_gets_a_guard() {
    // If a pattern silently lost its guard the crate would still be correct but
    // would quietly fall back to the slow backtracking path, which is the
    // regression this whole type exists to prevent.
    let options = NamingOptions::new();
    for pattern in vendored_patterns(&options) {
        let guarded = GuardedRegex::new(&pattern).expect("vendored pattern compiles");
        assert!(
            guarded.has_guard(),
            "no rejection guard built for {pattern:?}"
        );
    }
}

#[test]
fn guard_is_dropped_when_the_relaxed_pattern_cannot_be_built() {
    // A backreference survives stripping but the `regex` crate rejects it, so
    // the guard must be absent — and matching must still work.
    let guarded = GuardedRegex::new(r"(?<x>a+)b(?!c)\k<x>").expect("compiles on fancy-regex");
    assert!(!guarded.has_guard());
    assert!(guarded.is_match("aabaa").expect("runs"));
    assert!(!guarded.is_match("aabc").expect("runs"));
}
