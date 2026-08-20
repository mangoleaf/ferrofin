//! Port of `Emby.Naming.Video.Format3DParser`.

use crate::common::NamingOptions;
use crate::video::{Format3DResult, Format3DRule};

/// Parses 3D-format-related flags from a path.
#[must_use]
pub fn parse(path: &str, naming_options: &NamingOptions) -> Format3DResult {
    // Delimiters are the video flag delimiters plus a space.
    let mut delimiters = naming_options.video_flag_delimiters.clone();
    delimiters.push(' ');

    // The C# code walks a `ReadOnlySpan<char>`, so the split has to happen on
    // char (not byte) boundaries. Neither the decode nor the split depends on
    // the rule being tested — `start` advances by the same amount whatever the
    // rule is — so both are done once here instead of once per rule.
    let chars: Vec<char> = path.chars().collect();
    let tokens = split_tokens(&chars, &delimiters);

    for rule in &naming_options.format_3d_rules {
        let result = parse_rule(&tokens, rule);
        if result.is_3d {
            return result;
        }
    }

    Format3DResult::default()
}

/// Splits `chars` the way the C# loop does: on the next delimiter, or — when no
/// delimiter remains — on the last character (upstream's `Length - 1` fallback,
/// which deliberately drops that final char).
fn split_tokens<'a>(chars: &'a [char], delimiters: &[char]) -> Vec<&'a [char]> {
    let mut tokens = Vec::new();
    let mut start = 0usize;
    while start < chars.len() {
        let remaining = &chars[start..];
        let index = remaining
            .iter()
            .position(|c| delimiters.contains(c))
            .unwrap_or(remaining.len() - 1);
        tokens.push(&remaining[..index]);
        start += index + 1;
    }
    tokens
}

fn parse_rule(tokens: &[&[char]], rule: &Format3DRule) -> Format3DResult {
    // If there's no preceding token we just consider it found.
    let mut found_prefix = rule.preceding_token.as_deref().is_none_or(str::is_empty);

    for token in tokens {
        if !found_prefix {
            found_prefix = rule
                .preceding_token
                .as_deref()
                .is_some_and(|t| eq_ignore_ascii_case(token, t));
            continue;
        }

        if eq_ignore_ascii_case(token, &rule.token) {
            return Format3DResult::new(true, Some(rule.token.clone()));
        }
    }

    Format3DResult::default()
}

/// ASCII-case-insensitive equality between a `char` slice and a `str`,
/// equivalent to collecting the slice into a `String` and calling
/// [`str::eq_ignore_ascii_case`], but without allocating.
fn eq_ignore_ascii_case(slice: &[char], text: &str) -> bool {
    let mut expected = text.chars();
    for actual in slice {
        match expected.next() {
            Some(c) if actual.eq_ignore_ascii_case(&c) => {}
            _ => return false,
        }
    }
    expected.next().is_none()
}
