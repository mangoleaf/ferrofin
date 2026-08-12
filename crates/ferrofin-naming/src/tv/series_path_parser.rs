//! Port of `Emby.Naming.TV.SeriesPathParser`.

use crate::common::{EpisodeExpression, NamingOptions};
use crate::tv::SeriesPathParserResult;

/// Parses information about a series from a path.
///
/// Uses the same expressions as the episode parser but different success
/// criteria.
#[must_use]
pub fn parse(options: &NamingOptions, path: &str) -> SeriesPathParserResult {
    let mut result: Option<SeriesPathParserResult> = None;

    for expression in &options.episode_expressions {
        let current = parse_expression(path, expression);
        if current.success {
            result = Some(current);
            break;
        }
    }

    if let Some(res) = result.as_mut() {
        let trimmed = res
            .series_name
            .as_deref()
            .filter(|n| !n.is_empty())
            .map(|n| n.trim_matches([' ', '_', '.', '-']).to_string());
        if let Some(trimmed) = trimmed {
            res.series_name = Some(trimmed);
        }
    }

    result.unwrap_or_default()
}

fn parse_expression(name: &str, expression: &EpisodeExpression) -> SeriesPathParserResult {
    let mut result = SeriesPathParserResult::default();

    let Some(captures) = expression.regex().captures(name).ok().flatten() else {
        return result;
    };

    if captures.len() >= 3 && expression.is_named {
        let series_name = captures.name("seriesname").map(|m| m.as_str().to_string());
        let season_present = captures
            .name("seasonnumber")
            .is_some_and(|m| !m.as_str().is_empty());
        result.success = series_name.as_deref().is_some_and(|s| !s.is_empty()) && season_present;
        result.series_name = series_name;
    }

    result
}
