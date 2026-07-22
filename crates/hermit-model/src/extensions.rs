//! Port of `MediaBrowser.Model.Extensions` — `ContainerHelper`, `StringHelper`
//! and `EnumerableExtensions`.
//!
//! `LibraryOptionsExtension` depends on `LibraryOptions`
//! (`MediaBrowser.Model.Configuration`, a later port unit) and is deferred.

use hermit_util::split_string_extensions::span_split;

use crate::providers::RemoteImageInfo;

/// Splits a comma-delimited input string, discarding empty entries.
///
/// Mirrors `ContainerHelper.Split` (`String.Split(',',
/// StringSplitOptions.RemoveEmptyEntries)`). A `None`/empty input yields an
/// empty slice.
#[must_use]
pub fn split(input: Option<&str>) -> Vec<&str> {
    match input {
        None => Vec::new(),
        Some(s) => s.split(',').filter(|part| !part.is_empty()).collect(),
    }
}

/// Compares two containers, returning `true` if an item in `input_container`
/// exists in `profile_containers`.
///
/// Mirrors the two-argument `ContainerHelper.ContainsContainer(string?,
/// string?)`. If `profile_containers` begins with `-`, the operation is
/// reversed (negative list). If `profile_containers` is empty/`None`, all
/// containers are accepted.
#[must_use]
pub fn contains_container(profile_containers: Option<&str>, input_container: Option<&str>) -> bool {
    let (profile_containers, is_negative_list) = strip_negation(profile_containers);
    contains_container_with_negation(profile_containers, is_negative_list, input_container)
}

/// Compares two containers with an explicit negative-list flag.
///
/// Mirrors `ContainerHelper.ContainsContainer(string?, bool,
/// ReadOnlySpan<char>)`. Returns `is_negative_list` when no match is found (and
/// `!is_negative_list` when one is). An empty/`None` `input_container` returns
/// `is_negative_list`; an empty/`None` `profile_containers` returns `true`.
#[must_use]
pub fn contains_container_with_negation(
    profile_containers: Option<&str>,
    is_negative_list: bool,
    input_container: Option<&str>,
) -> bool {
    let input_container = input_container.unwrap_or("");
    if input_container.is_empty() {
        return is_negative_list;
    }

    let profile_containers = profile_containers.unwrap_or("");
    if profile_containers.is_empty() {
        // Empty profiles always support all containers/codecs.
        return true;
    }

    for container in input_container.split(',') {
        if !container.is_empty() {
            for profile in span_split(profile_containers, ',') {
                if !profile.is_empty() && container.eq_ignore_ascii_case(profile) {
                    return !is_negative_list;
                }
            }
        }
    }

    is_negative_list
}

/// Compares a list of profile containers against a comma-delimited input.
///
/// Mirrors `ContainerHelper.ContainsContainer(IReadOnlyList<string>?, bool,
/// string)`. A `None` `profile_containers` returns `true` (all accepted).
#[must_use]
pub fn contains_container_list(
    profile_containers: Option<&[&str]>,
    is_negative_list: bool,
    input_container: &str,
) -> bool {
    let Some(profile_containers) = profile_containers else {
        // Empty profiles always support all containers/codecs.
        return true;
    };

    for container in split(Some(input_container)) {
        for profile in profile_containers {
            if profile.eq_ignore_ascii_case(container) {
                return !is_negative_list;
            }
        }
    }

    is_negative_list
}

/// Strips a leading `-` negation marker from a profile-containers string,
/// returning the remaining string and whether it was a negative list.
fn strip_negation(profile_containers: Option<&str>) -> (Option<&str>, bool) {
    match profile_containers {
        Some(s) if s.starts_with('-') => (Some(&s[1..]), true),
        other => (other, false),
    }
}

/// Returns the string with the first character as uppercase.
///
/// Mirrors `StringHelper.FirstToUpper`. If the first character is not a
/// lowercase letter (checked via `is_lowercase`, so non-letters are left
/// unchanged), the string is returned as-is.
#[must_use]
pub fn first_to_upper(str: &str) -> String {
    let mut chars = str.chars();
    let Some(first) = chars.next() else {
        return String::new();
    };

    // We check is_lowercase instead of is_uppercase because both return false
    // for non-letters.
    if !first.is_lowercase() {
        return str.to_owned();
    }

    let mut result = String::with_capacity(str.len());
    for upper in first.to_uppercase() {
        result.push(upper);
    }
    result.push_str(chars.as_str());
    result
}

/// Orders remote image infos by requested language descending, then `en`, then
/// no language, over other non-matches.
///
/// Mirrors `EnumerableExtensions.OrderByLanguageDescending`. Within a language
/// priority tier, images are ordered by community rating (rounded to one
/// decimal) descending, then vote count descending. A blank
/// `requested_language` defaults to `en`.
///
/// The sort is stable (matching LINQ `OrderByDescending`/`ThenByDescending`),
/// so items comparing equal keep their original order.
#[must_use]
pub fn order_by_language_descending(
    remote_image_infos: impl IntoIterator<Item = RemoteImageInfo>,
    requested_language: &str,
) -> Vec<RemoteImageInfo> {
    let requested_language = if requested_language.trim().is_empty() {
        // Default to English if no requested language is specified.
        "en"
    } else {
        requested_language
    };

    let language_priority = |info: &RemoteImageInfo| -> i32 {
        // Image priority ordering:
        //  - Images that match the requested language
        //  - Images in English
        //  - Images with no language
        //  - Images that don't match the requested language
        match info.language.as_deref() {
            Some(lang) if lang.eq_ignore_ascii_case(requested_language) => 4,
            Some(lang) if lang.eq_ignore_ascii_case("en") => 3,
            None | Some("") => 2,
            Some(_) => 0,
        }
    };

    // Rounded community rating as an ordered integer key (one decimal place),
    // mirroring `Math.Round(x, 1)`.
    let rounded_rating = |info: &RemoteImageInfo| -> i64 {
        // Community ratings are small (0..=10); the rounded ×10 value fits i64.
        #[allow(clippy::cast_possible_truncation)]
        let scaled = (info.community_rating.unwrap_or(0.0) * 10.0).round() as i64;
        scaled
    };

    let mut result: Vec<RemoteImageInfo> = remote_image_infos.into_iter().collect();
    // Sort descending on the composite key; `sort_by` is stable, matching LINQ.
    result.sort_by(|a, b| {
        language_priority(b)
            .cmp(&language_priority(a))
            .then_with(|| rounded_rating(b).cmp(&rounded_rating(a)))
            .then_with(|| b.vote_count.unwrap_or(0).cmp(&a.vote_count.unwrap_or(0)))
    });
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // ----- ContainerHelper (ported from Dlna/ContainerHelperTests.cs) -----

    #[rstest]
    #[case("mp3,mpeg", Some("mp3"))]
    #[case("mp3,mpeg,avi", Some("mp3,avi"))]
    #[case("-mp3,mpeg", Some("avi"))]
    #[case("-mp3,mpeg,avi", Some("mp4,jpg"))]
    fn contains_container_in_list_returns_true(
        #[case] container: &str,
        #[case] extension: Option<&str>,
    ) {
        assert!(contains_container(Some(container), extension));
    }

    #[rstest]
    #[case("mp3,mpeg", Some("avi"))]
    #[case("mp3,mpeg,avi", Some("mp4,jpg"))]
    #[case("mp3,mpeg", None)]
    #[case("mp3,mpeg", Some(""))]
    #[case("-mp3,mpeg", Some("mp3"))]
    #[case("-mp3,mpeg,avi", Some("mpeg,avi"))]
    #[case(",mp3,", Some(",avi,"))] // Empty values should be discarded
    #[case("-,mp3,", Some(",mp3,"))] // Empty values should be discarded
    fn contains_container_not_in_list_returns_false(
        #[case] container: &str,
        #[case] extension: Option<&str>,
    ) {
        assert!(!contains_container(Some(container), extension));
    }

    #[rstest]
    #[case(&["mp3", "mpeg"], false, "mpeg")]
    #[case(&["mp3", "mpeg", "avi"], false, "avi")]
    #[case(&["mp3", "", "avi"], false, "mp3")]
    #[case(&["mp3", "mpeg"], true, "avi")]
    #[case(&["mp3", "mpeg", "avi"], true, "mkv")]
    #[case(&["mp3", "", "avi"], true, "")]
    fn contains_container_three_args_in_list_returns_true(
        #[case] containers: &[&str],
        #[case] is_negative_list: bool,
        #[case] input_container: &str,
    ) {
        assert!(contains_container_list(
            Some(containers),
            is_negative_list,
            input_container
        ));
    }

    #[rstest]
    #[case(&["mp3", "mpeg"], false, "avi")]
    #[case(&["mp3", "mpeg", "avi"], false, "mkv")]
    #[case(&["mp3", "", "avi"], false, "")]
    #[case(&["mp3", "mpeg"], true, "mpeg")]
    #[case(&["mp3", "mpeg", "avi"], true, "mp3")]
    #[case(&["mp3", "", "avi"], true, "avi")]
    fn contains_container_three_args_in_list_returns_false(
        #[case] containers: &[&str],
        #[case] is_negative_list: bool,
        #[case] input_container: &str,
    ) {
        assert!(!contains_container_list(
            Some(containers),
            is_negative_list,
            input_container
        ));
    }

    // ----- StringHelper (ported from Extensions/StringHelperTests.cs) -----

    #[rstest]
    #[case("", "")]
    #[case("banana", "Banana")]
    #[case("Banana", "Banana")]
    #[case("ä", "Ä")]
    #[case("\u{17}", "\u{17}")]
    fn string_helper_valid_args_success(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(expected, first_to_upper(input));
    }

    // ----- EnumerableExtensions (ported from EnumerableExtensionsTests.cs) -----

    fn img(language: Option<&str>, community_rating: f64, vote_count: i32) -> RemoteImageInfo {
        RemoteImageInfo {
            language: language.map(str::to_owned),
            community_rating: Some(community_rating),
            vote_count: Some(vote_count),
            ..RemoteImageInfo::default()
        }
    }

    #[test]
    fn order_by_language_descending_preferred_language_first() {
        let images = vec![
            img(Some("en"), 5.0, 100),
            img(Some("de"), 9.0, 200),
            img(None, 7.0, 50),
            img(Some("fr"), 8.0, 150),
        ];
        let result = order_by_language_descending(images, "de");
        assert_eq!(Some("de"), result[0].language.as_deref());
        assert_eq!(Some("en"), result[1].language.as_deref());
        assert_eq!(None, result[2].language.as_deref());
        assert_eq!(Some("fr"), result[3].language.as_deref());
    }

    #[test]
    fn order_by_language_descending_english_before_no_language() {
        let images = vec![img(None, 9.0, 500), img(Some("en"), 3.0, 10)];
        let result = order_by_language_descending(images, "de");
        // English should come before no-language, even with lower rating.
        assert_eq!(Some("en"), result[0].language.as_deref());
        assert_eq!(None, result[1].language.as_deref());
    }

    #[test]
    fn order_by_language_descending_same_language_sorted_by_rating_then_vote_count() {
        let images = vec![
            img(Some("de"), 5.0, 100),
            img(Some("de"), 9.0, 50),
            img(Some("de"), 9.0, 200),
        ];
        let result = order_by_language_descending(images, "de");
        assert_eq!(Some(200), result[0].vote_count);
        assert_eq!(Some(50), result[1].vote_count);
        assert_eq!(Some(100), result[2].vote_count);
    }

    #[test]
    fn order_by_language_descending_null_requested_language_defaults_to_english() {
        let images = vec![img(Some("fr"), 9.0, 500), img(Some("en"), 5.0, 10)];
        let result = order_by_language_descending(images, "");
        // With null/blank requested language, English becomes preferred (score 4).
        assert_eq!(Some("en"), result[0].language.as_deref());
        assert_eq!(Some("fr"), result[1].language.as_deref());
    }

    #[test]
    fn order_by_language_descending_english_requested_no_double_boost() {
        let images = vec![
            img(None, 9.0, 500),
            img(Some("en"), 3.0, 10),
            img(Some("fr"), 8.0, 300),
        ];
        let result = order_by_language_descending(images, "en");
        assert_eq!(Some("en"), result[0].language.as_deref());
        assert_eq!(None, result[1].language.as_deref());
        assert_eq!(Some("fr"), result[2].language.as_deref());
    }

    #[test]
    fn order_by_language_descending_full_priority_order() {
        let images = vec![
            img(Some("fr"), 9.0, 500),
            img(None, 8.0, 400),
            img(Some("en"), 7.0, 300),
            img(Some("de"), 6.0, 200),
        ];
        let result = order_by_language_descending(images, "de");
        // Expected order: de (requested) > en > no-language > fr (other).
        assert_eq!(Some("de"), result[0].language.as_deref());
        assert_eq!(Some("en"), result[1].language.as_deref());
        assert_eq!(None, result[2].language.as_deref());
        assert_eq!(Some("fr"), result[3].language.as_deref());
    }
}
