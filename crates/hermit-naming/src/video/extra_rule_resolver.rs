//! Port of `Emby.Naming.Video.ExtraRuleResolver`.

use regex::Regex;

use crate::audio;
use crate::common::{MediaType, NamingOptions};
use crate::path;
use crate::video::{ExtraResult, ExtraRuleType, is_video_file};

const DIGITS: &[char] = &['0', '1', '2', '3', '4', '5', '6', '7', '8', '9'];

/// Attempts to resolve whether a file is an extra.
#[must_use]
pub fn get_extra_info(
    path_str: &str,
    naming_options: &NamingOptions,
    library_root: Option<&str>,
) -> ExtraResult {
    let library_root = library_root.unwrap_or("");
    let mut result = ExtraResult::default();

    let is_audio_file = audio::is_audio_file(path_str, naming_options);
    let is_video_file = is_video_file(path_str, naming_options);

    let file_name = path::file_name(path_str);
    let file_name_without_extension = path::file_name_without_extension(path_str);
    // Trim trailing digits so things like `-trailer2` are recognised.
    let trimmed_file_name_without_extension = file_name_without_extension.trim_end_matches(DIGITS);
    let full_directory = path::directory_name(path_str).unwrap_or("");
    let directory_name = path::file_name(full_directory);

    for rule in &naming_options.video_extra_rules {
        if (rule.media_type == MediaType::Audio && !is_audio_file)
            || (rule.media_type == MediaType::Video && !is_video_file)
        {
            continue;
        }

        let is_match = match rule.rule_type {
            ExtraRuleType::Filename => {
                file_name_without_extension.eq_ignore_ascii_case(&rule.token)
            }
            ExtraRuleType::Suffix => {
                ends_with_ignore_ascii_case(trimmed_file_name_without_extension, &rule.token)
            }
            ExtraRuleType::Regex => {
                Regex::new(&format!("(?i){}", rule.token)).is_ok_and(|re| re.is_match(file_name))
            }
            ExtraRuleType::DirectoryName => {
                directory_name.eq_ignore_ascii_case(&rule.token)
                    && !full_directory.eq_ignore_ascii_case(library_root)
            }
        };

        if !is_match {
            continue;
        }

        result.extra_type = Some(rule.extra_type);
        result.rule = Some(rule.clone());
        return result;
    }

    result
}

fn ends_with_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let Some(start) = haystack.len().checked_sub(needle.len()) else {
        return false;
    };
    haystack
        .get(start..)
        .is_some_and(|tail| tail.eq_ignore_ascii_case(needle))
}
