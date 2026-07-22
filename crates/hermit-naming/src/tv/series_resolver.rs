//! Port of `Emby.Naming.TV.SeriesResolver`.

use std::sync::OnceLock;

use regex::Regex;

use crate::common::NamingOptions;
use crate::path;
use crate::tv::{SeriesInfo, series_path_parser};

/// Matches at-least-2-char words separated by dots/underscores, so `The_show`
/// becomes `The show` while `S.H.O.W` is preserved.
fn series_name_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"((?<a>[^\._]{2,})[\._]*)|([\._](?<b>[^\._]{2,}))")
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
        series_name = series_name_regex()
            .replace_all(&series_name, "${a} ${b}")
            .trim()
            .to_string();
    }

    let mut info = SeriesInfo::new(path_str);
    info.name = Some(series_name);
    info
}
