//! Port of `Emby.Naming.ExternalFiles.ExternalPathParser`.

use ferrofin_model::dlna::DlnaProfileType;

use crate::common::NamingOptions;
use crate::external_files::{ExternalPathParserResult, LocalizationManager};
use crate::path;

/// Length of a single media-flag delimiter (always one char).
const SEPARATOR_LENGTH: usize = 1;

/// External media file parser.
pub struct ExternalPathParser<'a, L: LocalizationManager> {
    naming_options: &'a NamingOptions,
    localization_manager: &'a L,
    profile_type: DlnaProfileType,
}

impl<'a, L: LocalizationManager> ExternalPathParser<'a, L> {
    /// Creates a new [`ExternalPathParser`].
    #[must_use]
    pub fn new(
        naming_options: &'a NamingOptions,
        localization_manager: &'a L,
        profile_type: DlnaProfileType,
    ) -> Self {
        Self {
            naming_options,
            localization_manager,
            profile_type,
        }
    }

    /// Parses a filename and extracts external-file information.
    #[must_use]
    #[allow(clippy::too_many_lines)]
    pub fn parse_file(
        &self,
        path_str: &str,
        extra_string: Option<&str>,
    ) -> Option<ExternalPathParserResult> {
        if path_str.is_empty() {
            return None;
        }

        let extension = path::extension(path_str);
        let matches_extension =
            |exts: &[String]| exts.iter().any(|e| e.eq_ignore_ascii_case(extension));

        let supported = match self.profile_type {
            DlnaProfileType::Subtitle => {
                matches_extension(&self.naming_options.subtitle_file_extensions)
            }
            DlnaProfileType::Audio => matches_extension(&self.naming_options.audio_file_extensions),
            DlnaProfileType::Lyric => matches_extension(&self.naming_options.lyric_file_extensions),
            _ => false,
        };
        if !supported {
            return None;
        }

        let mut path_info = ExternalPathParserResult::new(path_str);

        let mut extra_string = match extra_string {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return Some(path_info),
        };

        for &separator in &self.naming_options.media_flag_delimiters {
            let mut language_string = extra_string.clone();
            let mut title_string = String::new();

            while !language_string.is_empty() {
                let Some(last_separator) = language_string.rfind(separator) else {
                    break;
                };

                let current_slice = language_string[last_separator..].to_string();
                let current_slice_without_separator = current_slice[SEPARATOR_LENGTH..].to_string();

                if self
                    .naming_options
                    .media_default_flags
                    .iter()
                    .any(|s| contains_ignore_ascii_case(&current_slice_without_separator, s))
                {
                    path_info.is_default = true;
                    extra_string = replace_ignore_case(&extra_string, &current_slice, "");
                    language_string = language_string[..last_separator].to_string();
                    continue;
                }

                if self
                    .naming_options
                    .media_forced_flags
                    .iter()
                    .any(|s| contains_ignore_ascii_case(&current_slice_without_separator, s))
                {
                    path_info.is_forced = true;
                    extra_string = replace_ignore_case(&extra_string, &current_slice, "");
                    language_string = language_string[..last_separator].to_string();
                    continue;
                }

                // Try to translate to a three-character code.
                let culture = self
                    .localization_manager
                    .find_language_info(&current_slice_without_separator);

                if let Some(culture) = culture.as_ref().filter(|_| path_info.language.is_none()) {
                    path_info.language = Some(resolve_language(culture));
                    extra_string = replace_ignore_case(&extra_string, &current_slice, "");
                } else if let Some(culture) = culture
                    .as_ref()
                    .filter(|_| path_info.language.as_deref() == Some("hin"))
                {
                    // "hi" collides with a hearing-impaired flag; only Hindi if
                    // no other language is set.
                    path_info.is_hearing_impaired = true;
                    path_info.language = Some(resolve_language(culture));
                    extra_string = replace_ignore_case(&extra_string, &current_slice, "");
                } else if self
                    .naming_options
                    .media_hearing_impaired_flags
                    .iter()
                    .any(|s| current_slice_without_separator.eq_ignore_ascii_case(s))
                {
                    path_info.is_hearing_impaired = true;
                    extra_string = replace_ignore_case(&extra_string, &current_slice, "");
                } else {
                    title_string = format!("{current_slice}{title_string}");
                }

                language_string = language_string[..last_separator].to_string();
            }

            path_info.title = if title_string.len() >= SEPARATOR_LENGTH {
                Some(title_string[SEPARATOR_LENGTH..].to_string())
            } else {
                None
            };
        }

        Some(path_info)
    }
}

/// Mirrors `culture.Name.Contains('-') ? culture.Name : ThreeLetterISOName`.
fn resolve_language(culture: &ferrofin_model::globalization::CultureDto) -> String {
    if culture.name.contains('-') {
        culture.name.clone()
    } else {
        culture
            .three_letter_iso_language_name
            .clone()
            .or_else(|| culture.three_letter_iso_language_names.first().cloned())
            .unwrap_or_default()
    }
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack
        .to_ascii_lowercase()
        .contains(&needle.to_ascii_lowercase())
}

/// Replaces every case-insensitive occurrence of `needle` in `haystack`.
fn replace_ignore_case(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }
    let haystack_lower = haystack.to_ascii_lowercase();
    let needle_lower = needle.to_ascii_lowercase();
    let mut result = String::with_capacity(haystack.len());
    let mut search_from = 0;
    while let Some(rel) = haystack_lower[search_from..].find(&needle_lower) {
        let start = search_from + rel;
        result.push_str(&haystack[search_from..start]);
        result.push_str(replacement);
        search_from = start + needle.len();
    }
    result.push_str(&haystack[search_from..]);
    result
}
