//! Port of `Emby.Naming.Video.Format3DParser`.

use crate::common::NamingOptions;
use crate::video::{Format3DResult, Format3DRule};

/// Parses 3D-format-related flags from a path.
#[must_use]
pub fn parse(path: &str, naming_options: &NamingOptions) -> Format3DResult {
    // Delimiters are the video flag delimiters plus a space.
    let mut delimiters = naming_options.video_flag_delimiters.clone();
    delimiters.push(' ');

    for rule in &naming_options.format_3d_rules {
        let result = parse_rule(path, rule, &delimiters);
        if result.is_3d {
            return result;
        }
    }

    Format3DResult::default()
}

fn parse_rule(path: &str, rule: &Format3DRule, delimiters: &[char]) -> Format3DResult {
    let mut is_3d = false;
    let mut format_3d: Option<String> = None;

    // If there's no preceding token we just consider it found.
    let mut found_prefix = rule.preceding_token.as_deref().is_none_or(str::is_empty);

    // Work over char indices to mirror the C# ReadOnlySpan<char> slicing.
    let chars: Vec<char> = path.chars().collect();
    let mut start = 0usize;

    while start < chars.len() {
        let remaining = &chars[start..];
        let index = remaining
            .iter()
            .position(|c| delimiters.contains(c))
            .unwrap_or(remaining.len() - 1);

        let current_slice: String = remaining[..index].iter().collect();
        start += index + 1;

        if !found_prefix {
            found_prefix = rule
                .preceding_token
                .as_deref()
                .is_some_and(|t| current_slice.eq_ignore_ascii_case(t));
            continue;
        }

        is_3d = found_prefix && current_slice.eq_ignore_ascii_case(&rule.token);

        if is_3d {
            format_3d = Some(rule.token.clone());
            break;
        }
    }

    if is_3d {
        Format3DResult::new(true, format_3d)
    } else {
        Format3DResult::default()
    }
}
