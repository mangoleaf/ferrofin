//! Port of `Emby.Naming.Video.CleanStringParser`.

use fancy_regex::Regex;

/// Attempts to extract a clean name using the supplied regular expressions.
///
/// Returns `Some(cleaned)` when at least one expression cleaned the string,
/// mirroring the C# `TryClean` out-param + bool return.
#[must_use]
pub fn try_clean(name: Option<&str>, expressions: &[Regex]) -> Option<String> {
    let name = name?;
    if name.is_empty() {
        return None;
    }

    // Iteratively apply the regexes to clean the string.
    let mut cleaned = false;
    let mut current = name.to_string();
    for expression in expressions {
        if let Some(new_name) = try_clean_one(&current, expression) {
            cleaned = true;
            current = new_name;
        }
    }

    if cleaned { Some(current) } else { None }
}

fn try_clean_one(name: &str, expression: &Regex) -> Option<String> {
    let captures = expression.captures(name).ok().flatten()?;
    let cleaned = captures.name("cleaned")?;
    Some(cleaned.as_str().trim().to_string())
}
