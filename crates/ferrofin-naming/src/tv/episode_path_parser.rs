//! Port of `Emby.Naming.TV.EpisodePathParser`.

use chrono::NaiveDate;

use crate::common::{EpisodeExpression, NamingOptions};
use crate::tv::EpisodePathParserResult;

/// Parses information about an episode from a path.
pub struct EpisodePathParser<'a> {
    options: &'a NamingOptions,
}

impl<'a> EpisodePathParser<'a> {
    /// Creates a new [`EpisodePathParser`].
    #[must_use]
    pub fn new(options: &'a NamingOptions) -> Self {
        Self { options }
    }

    /// Parses information about an episode from a path.
    ///
    /// `is_named`, `is_optimistic` and `supports_absolute_numbers` filter which
    /// expressions are considered (`None` = don't filter).
    #[must_use]
    pub fn parse(
        &self,
        path: &str,
        is_directory: bool,
        is_named: Option<bool>,
        is_optimistic: Option<bool>,
        supports_absolute_numbers: Option<bool>,
        fill_extended_info: bool,
    ) -> EpisodePathParserResult {
        // Some regexes require a file extension; add one for directories.
        let mut owned_path = path.to_string();
        if is_directory {
            owned_path.push_str(".mp4");
        }
        let path = owned_path.as_str();

        let mut result: Option<EpisodePathParserResult> = None;

        for expression in &self.options.episode_expressions {
            if supports_absolute_numbers
                .is_some_and(|v| expression.supports_absolute_episode_numbers != v)
                || is_named.is_some_and(|v| expression.is_named != v)
                || is_optimistic.is_some_and(|v| expression.is_optimistic != v)
            {
                continue;
            }

            let current = Self::parse_expression(path, expression);
            if current.success {
                result = Some(current);
                break;
            }
        }

        if let Some(res) = result.as_mut().filter(|_| fill_extended_info) {
            self.fill_additional(path, res);

            if let Some(series_name) = res.series_name.as_deref().filter(|s| !s.is_empty()) {
                res.series_name = Some(
                    series_name
                        .trim()
                        .trim_matches(['_', '.', '-'])
                        .trim()
                        .to_string(),
                );
            }
        }

        result.unwrap_or_default()
    }

    /// Convenience wrapper mirroring the common two-argument C# call.
    #[must_use]
    pub fn parse_simple(&self, path: &str, is_directory: bool) -> EpisodePathParserResult {
        self.parse(path, is_directory, None, None, None, true)
    }

    fn parse_expression(name: &str, expression: &EpisodeExpression) -> EpisodePathParserResult {
        let mut result = EpisodePathParserResult::default();

        // Hack to handle wmc naming: by-date expressions treat '_' as '-'.
        let owned;
        let name: &str = if expression.is_by_date {
            owned = name.replace('_', "-");
            &owned
        } else {
            name
        };

        let Some(captures) = expression.regex().captures(name).ok().flatten() else {
            return result;
        };

        // (Full)(Season)(Episode)(Extension)
        if captures.len() >= 3 {
            if expression.is_by_date {
                let parsed = captures.get(0).and_then(|whole| {
                    let value = whole.as_str();
                    if expression.date_time_formats.is_empty() {
                        try_parse_general(value)
                    } else {
                        try_parse_exact(value, &expression.date_time_formats)
                    }
                });
                if let Some((y, m, d)) = parsed {
                    result.year = Some(y);
                    result.month = Some(m);
                    result.day = Some(d);
                    result.success = true;
                }

                // TODO(upstream): only consider success if the date parsed.
                result.success = true;
            } else if expression.is_named {
                result.season_number = capture_i32(&captures, "seasonnumber");
                result.episode_number = capture_i32(&captures, "epnumber");

                if let Some(ending) = captures.name("endingepnumber") {
                    // Only set EndingEpisodeNumber if not followed by more
                    // digits or a pixel-resolution 'p'/'i'. Avoids parsing
                    // "series-s09e14-1080p" as E14–E108.
                    let next_index = ending.end();
                    let next_char = name[next_index..].chars().next();
                    let follows_resolution =
                        next_char.is_some_and(|c| matches!(c, '0'..='9' | 'i' | 'I' | 'p' | 'P'));
                    if !follows_resolution {
                        result.ending_episode_number = ending.as_str().parse::<i32>().ok();
                    }
                }

                result.series_name = captures.name("seriesname").map(|m| m.as_str().to_string());
                result.success = result.episode_number.is_some();
            } else {
                result.season_number = captures.get(1).and_then(|g| g.as_str().parse::<i32>().ok());
                result.episode_number =
                    captures.get(2).and_then(|g| g.as_str().parse::<i32>().ok());
                result.success = result.episode_number.is_some();
            }

            // Invalidate seasons 200–1927 or above 2500 (false positives from
            // resolutions like "1920x1080").
            if result
                .season_number
                .is_some_and(|s| (200..1928).contains(&s) || s > 2500)
            {
                result.success = false;
            }

            result.is_by_date = expression.is_by_date;
        }

        result
    }

    fn fill_additional(&self, path: &str, info: &mut EpisodePathParserResult) {
        let mut expressions: Vec<&EpisodeExpression> = self
            .options
            .multiple_episode_expressions
            .iter()
            .filter(|i| i.is_named)
            .collect();

        if info.series_name.as_deref().unwrap_or("").is_empty() {
            let named: Vec<&EpisodeExpression> = self
                .options
                .episode_expressions
                .iter()
                .filter(|i| i.is_named)
                .collect();
            let mut combined = named;
            combined.extend(expressions);
            expressions = combined;
        }

        Self::fill_additional_from(path, info, &expressions);
    }

    fn fill_additional_from(
        path: &str,
        info: &mut EpisodePathParserResult,
        expressions: &[&EpisodeExpression],
    ) {
        for expression in expressions {
            let result = Self::parse_expression(path, expression);

            if !result.success {
                continue;
            }

            if info.series_name.as_deref().unwrap_or("").is_empty() {
                info.series_name.clone_from(&result.series_name);
            }

            if info.ending_episode_number.is_none() && info.episode_number.is_some() {
                info.ending_episode_number = result.ending_episode_number;
            }

            if !info.series_name.as_deref().unwrap_or("").is_empty()
                && (info.episode_number.is_none() || info.ending_episode_number.is_some())
            {
                break;
            }
        }
    }
}

fn capture_i32(captures: &fancy_regex::Captures<'_>, name: &str) -> Option<i32> {
    captures.name(name)?.as_str().parse::<i32>().ok()
}

/// Converts a `.NET` custom date format (e.g. `yyyy.MM.dd`) to a chrono format.
fn to_chrono_format(net_format: &str) -> String {
    net_format
        .replace("yyyy", "%Y")
        .replace("MM", "%m")
        .replace("dd", "%d")
}

fn try_parse_exact(value: &str, formats: &[String]) -> Option<(i32, i32, i32)> {
    for format in formats {
        let chrono_format = to_chrono_format(format);
        if let Ok(date) = NaiveDate::parse_from_str(value, &chrono_format) {
            return Some(ymd(date));
        }
    }
    None
}

/// Mirrors the general `DateTime.TryParse` fallback for `yyyy-MM-dd HH:mm:ss`
/// style values reached only by the empty-date-parsers path.
fn try_parse_general(value: &str) -> Option<(i32, i32, i32)> {
    // Take the leading date portion before any time component.
    let date_part = value.split([' ', 'T']).next().unwrap_or(value);
    for format in ["%Y-%m-%d", "%Y.%m.%d", "%Y_%m_%d"] {
        if let Ok(date) = NaiveDate::parse_from_str(date_part, format) {
            return Some(ymd(date));
        }
    }
    None
}

fn ymd(date: NaiveDate) -> (i32, i32, i32) {
    use chrono::Datelike;
    #[allow(clippy::cast_possible_wrap)]
    (date.year(), date.month() as i32, date.day() as i32)
}
