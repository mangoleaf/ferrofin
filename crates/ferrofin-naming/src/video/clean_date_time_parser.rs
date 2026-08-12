//! Port of `Emby.Naming.Video.CleanDateTimeParser`.
//!
//! See <http://kodi.wiki/view/Advancedsettings.xml#video>.

use fancy_regex::Regex;

use crate::video::CleanDateTimeResult;

/// Attempts to clean the name, extracting a trailing year.
#[must_use]
pub fn clean(name: &str, clean_date_time_regexes: &[Regex]) -> CleanDateTimeResult {
    let mut result = CleanDateTimeResult::new(name, None);
    if name.is_empty() {
        return result;
    }

    for expression in clean_date_time_regexes {
        if try_clean(name, expression, &mut result) {
            return result;
        }
    }

    result
}

fn try_clean(name: &str, expression: &Regex, result: &mut CleanDateTimeResult) -> bool {
    let Some(captures) = expression.captures(name).ok().flatten() else {
        return false;
    };

    // C# checks Groups.Count == 5 (group 0 + 4 capture groups) and that the
    // first two capture groups succeeded, then parses group 2 as the year.
    if captures.len() != 5 {
        return false;
    }

    let (Some(group1), Some(group2)) = (captures.get(1), captures.get(2)) else {
        return false;
    };

    let Ok(year) = group2.as_str().parse::<i32>() else {
        return false;
    };

    *result = CleanDateTimeResult::new(group1.as_str().trim_end(), Some(year));
    true
}
