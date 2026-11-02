//! Port of `Emby.Naming.TV.SeriesResolver`.

use std::sync::OnceLock;

use regex::Regex;

use crate::common::NamingOptions;
use crate::path;
use crate::tv::{SeriesInfo, series_path_parser};

/// Matches a run of dots or underscores that separates two words, where a
/// word is at least 2 characters long, so `The_show` becomes `The show` while
/// acronyms like `S.H.O.W` — whose single letters are a word on neither side —
/// keep their dots. Whitespace bounds a word too, so the dot in
/// `Marvel's Agents of S.H.I.E.L.D.` is read against the `S` beside it
/// (upstream PR #17858; the lookbehind needs `fancy_regex`).
fn series_name_regex() -> &'static fancy_regex::Regex {
    static RE: OnceLock<fancy_regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        fancy_regex::Regex::new(r"(?<=[^\s\._]{2})[\._]+|[\._]+(?=[^\s\._]{2})")
            .expect("series name regex valid")
    })
}

/// Matches titles with a year in parentheses (title may be numeric).
fn title_with_year_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?<title>.+?)\s*\((?<year>[0-9]{4})\)").expect("title-with-year regex valid")
    })
}

/// Resolves information about a series from a path.
#[must_use]
pub fn resolve(options: &NamingOptions, path_str: &str) -> SeriesInfo {
    let mut series_name = path::file_name(path_str).to_string();

    // First check for a "title (year)" pattern (handles numeric titles).
    let year_match = if series_name.is_empty() {
        None
    } else {
        title_with_year_regex().captures(&series_name)
    };
    if let Some(caps) = year_match {
        let title = caps.name("title").map_or("", |m| m.as_str()).trim();
        let year = caps
            .name("year")
            .and_then(|m| m.as_str().parse::<i32>().ok());
        let mut info = SeriesInfo::new(path_str);
        info.name = Some(title.to_string());
        info.year = year;
        return info;
    }

    let result = series_path_parser::parse(options, path_str);
    if let Some(name) = result
        .series_name
        .filter(|n| result.success && !n.is_empty())
    {
        series_name = name;
    }

    if !series_name.is_empty() {
        // A backtracking-limit error leaves the name unreplaced rather than
        // failing the resolve.
        series_name = series_name_regex()
            .try_replacen(&series_name, 0, " ")
            .map_or_else(
                |_| series_name.trim().to_string(),
                |name| name.trim().to_string(),
            );
    }

    let mut info = SeriesInfo::new(path_str);
    info.name = Some(series_name);
    info
}
