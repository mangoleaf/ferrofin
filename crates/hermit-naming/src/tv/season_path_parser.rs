//! Port of `Emby.Naming.TV.SeasonPathParser`.

use std::sync::OnceLock;

use fancy_regex::Regex;
use regex::Regex as PlainRegex;

use crate::path;
use crate::tv::SeasonPathParserResult;

const SEASON_KEYWORD_PATTERN: &str = concat!(
    "시즌|シーズン|сезон",
    "|season|sæson|saison|staffel|series|stagione|säsong|seizoen|seasong",
    "|sezon|sezona|sezóna|sezonul|série|séria|serie|seria|temporada|kausi",
);

fn clean_name_regex() -> &'static PlainRegex {
    static RE: OnceLock<PlainRegex> = OnceLock::new();
    RE.get_or_init(|| PlainRegex::new(r"[ ._\-\[\]]").expect("clean name regex valid"))
}

fn process_pre() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?i)^\s*((?<seasonnumber>(?>\d+))(?:st|nd|rd|th|\.)*(?!\s*[Ee]\d+))\s*(?:{SEASON_KEYWORD_PATTERN})\s*(?<rightpart>.*)$"
        ))
        .expect("process pre regex valid")
    })
}

fn process_post() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!(
            r"(?i)^\s*(?:{SEASON_KEYWORD_PATTERN})\s*(?<seasonnumber>\d+?)(?=\d{{3,4}}p|[^\d]|$)(?!\s*[Ee]\d)(?<rightpart>.*)$"
        ))
        .expect("process post regex valid")
    })
}

fn season_prefix() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"[sS](\d{1,4})(?!\d|[eE]\d)(?=\.|_|-|\[|\]|\s|$)")
            .expect("season prefix regex valid")
    })
}

fn season_keyword() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(&format!("(?i){SEASON_KEYWORD_PATTERN}")).expect("season keyword regex valid")
    })
}

/// Attempts to parse a season number from a path.
#[must_use]
pub fn parse(
    path_str: &str,
    parent_path: Option<&str>,
    support_special_aliases: bool,
    support_numeric_season_folders: bool,
) -> SeasonPathParserResult {
    let mut result = SeasonPathParserResult::default();
    let parent_folder_name = parent_path.map(path::file_name);

    let (season_number, is_season_folder) = get_season_number_from_path(
        path_str,
        parent_folder_name,
        support_special_aliases,
        support_numeric_season_folders,
    );

    result.season_number = season_number;

    if result.season_number.is_some() {
        result.success = true;
        result.is_season_folder = is_season_folder;
    }

    result
}

fn get_season_number_from_path(
    path_str: &str,
    parent_folder_name: Option<&str>,
    support_special_aliases: bool,
    support_numeric_season_folders: bool,
) -> (Option<i32>, bool) {
    let file_name = path::file_name(path_str);

    let prefix_val = season_prefix()
        .captures(file_name)
        .ok()
        .flatten()
        .and_then(|caps| caps.get(1).and_then(|m| m.as_str().parse::<i32>().ok()));
    if let Some(val) = prefix_val {
        return (Some(val), true);
    }

    let mut cleaned = clean_name_regex().replace_all(file_name, "").into_owned();

    if let Some(parent) = parent_folder_name {
        let clean_parent = clean_name_regex().replace_all(parent, "").into_owned();
        cleaned = replace_ignore_case(&cleaned, &clean_parent, "");
    }

    if support_special_aliases
        && (cleaned.eq_ignore_ascii_case("specials") || cleaned.eq_ignore_ascii_case("extras"))
    {
        return (Some(0), true);
    }

    if let Some(val) = cleaned
        .parse::<i32>()
        .ok()
        .filter(|_| support_numeric_season_folders)
    {
        return (Some(val), true);
    }

    let is_mixed_library = !support_numeric_season_folders && !support_special_aliases;

    if let Ok(Some(pre_match)) = process_pre().captures(&cleaned) {
        if is_mixed_library && !is_keyword_match(file_name) {
            return (None, false);
        }
        return check_match(&pre_match);
    }

    if let Ok(Some(post_match)) = process_post().captures(&cleaned) {
        if is_mixed_library && !is_keyword_match(file_name) {
            return (None, false);
        }
        return check_match(&post_match);
    }

    (None, false)
}

fn is_keyword_match(text: &str) -> bool {
    season_keyword().is_match(text).unwrap_or(false)
}

fn check_match(captures: &fancy_regex::Captures<'_>) -> (Option<i32>, bool) {
    match captures
        .name("seasonnumber")
        .and_then(|m| m.as_str().parse::<i32>().ok())
    {
        Some(season_number) => (Some(season_number), true),
        None => (None, false),
    }
}

/// Replaces every case-insensitive occurrence of `needle` in `haystack` with
/// `replacement`, mirroring C# `string.Replace(_, _, OrdinalIgnoreCase)`.
fn replace_ignore_case(haystack: &str, needle: &str, replacement: &str) -> String {
    if needle.is_empty() {
        return haystack.to_string();
    }

    // ASCII-lowercase both sides so byte offsets stay aligned with the
    // original strings (C# uses OrdinalIgnoreCase, which for these inputs is
    // effectively case-insensitive ASCII).
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
