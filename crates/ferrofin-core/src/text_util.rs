//! Small text-normalization helpers shared by the item repository.
//!
//! Port of Jellyfin 12.0's `GetCleanValue` string extension. The item
//! repository stores a normalized `CleanName` / `CleanValue` alongside every
//! display name so that name, genre, tag, and artist lookups are
//! diacritic- and punctuation-insensitive; the query translator recomputes the
//! same normalization on the filter values so they compare against the stored
//! clean columns.

/// Normalizes a display value into its stored "clean" form.
///
/// Exactly C# `StringExtensions.GetCleanValue` on Jellyfin 12.0
/// (`src/Jellyfin.Extensions/StringExtensions.cs:159-176`):
///
/// ```text
/// if (string.IsNullOrWhiteSpace(value)) return value;
/// var cleaned = value.RemoveDiacritics().ToLowerInvariant();
/// cleaned = Regex.Replace(cleaned, @"[^\p{L}\p{N}\s]", " ");
/// cleaned = Regex.Replace(cleaned, @"\s+", " ").Trim();
/// ```
///
/// Used at write time (`upsert_item`, the by-name inserts, `save_item_values`)
/// **and** at query time (`translate_query`, the DTO caches) — the two must
/// agree or every by-name lookup misses. 12.0 uses it in the same places
/// (`BaseItemMapper.cs:245`, `ItemPersistenceService.cs:317`,
/// `TranslateQuery.cs:262,486,668-682`, `SqlSearchProvider.cs:90`).
///
/// A database written under 10.11.8's rule (fold + lower-case only, so
/// `'h. jon benjamin'`) is moved to this form once by
/// [`FerrofinItemPersistenceService::repair_clean_values`](crate::item_persistence_service::FerrofinItemPersistenceService::repair_clean_values),
/// the port of 12.0's `RefreshCleanNamesAndValues` routine.
#[must_use]
pub fn get_clean_value(value: &str) -> String {
    ferrofin_util::string_extensions::get_clean_value(value)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::get_clean_value;

    #[test]
    fn lowercases_and_strips_diacritics() {
        assert_eq!(get_clean_value("Amélie"), "amelie");
        assert_eq!(get_clean_value("Motörhead"), "motorhead");
    }

    /// 12.0's `RefreshCleanNamesAndValues` rewrites the 10.11.8 spellings
    /// (`'h. jon benjamin'`, `'spider-man: across the spider-verse'`) to these.
    #[rstest]
    #[case("dune: part two", "dune part two")]
    #[case("weird: the al yankovic story", "weird the al yankovic story")]
    #[case("Mötley Crüe: Greatest Hits!", "motley crue greatest hits")]
    #[case("H. Jon Benjamin", "h jon benjamin")]
    #[case(
        "Spider-Man: Across the Spider-Verse",
        "spider man across the spider verse"
    )]
    #[case("Warner Bros. Pictures", "warner bros pictures")]
    #[case("2001: A Space Odyssey", "2001 a space odyssey")]
    #[case("千と千尋の神隠し", "千と千尋の神隠し")]
    #[case("千と千尋の神隠し（2001）", "千と千尋の神隠し 2001")]
    fn punctuation_becomes_a_single_space(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(get_clean_value(input), expected);
    }

    #[test]
    fn blank_input_is_returned_unchanged() {
        assert_eq!(get_clean_value(""), "");
        assert_eq!(get_clean_value("   "), "   ");
    }
}
