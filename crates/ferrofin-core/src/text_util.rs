//! Small text-normalization helpers shared by the item repository.
//!
//! Port of the `GetCleanValue` extension method in Jellyfin's
//! `BaseItemRepository`. The item repository stores a normalized `CleanName` /
//! `CleanValue` alongside every display name so that name, genre, tag, and
//! artist lookups are diacritic-insensitive; the query translator recomputes
//! the same normalization on the filter values so they compare against the
//! stored clean columns.

/// Normalizes a display value into its stored "clean" form.
///
/// Exactly C# `BaseItemRepository.GetCleanValue`
/// (`BaseItemRepository.cs:1438`):
///
/// ```text
/// if (string.IsNullOrWhiteSpace(value)) return value;
/// return value.RemoveDiacritics().ToLowerInvariant();
/// ```
///
/// **Punctuation is kept.** Ferrofin used to also replace every
/// non-alphanumeric character with a space and collapse the runs, which reads
/// plausible and is wrong: a real Jellyfin 10.11.8 database stores
/// `CleanName` `'h. jon benjamin'`, `'spider-man: across the spider-verse'`
/// and `CleanValue` `'warner bros. pictures'`. Stripping the punctuation made
/// every by-name lookup (person, studio, genre, tag) miss on an adopted
/// database — `/Persons` resolved one name out of five, because
/// `"H. Jon Benjamin"` was looked up as `h jon benjamin`.
#[must_use]
pub fn get_clean_value(value: &str) -> String {
    ferrofin_util::string_extensions::get_clean_value(value)
}

#[cfg(test)]
mod tests {
    use super::get_clean_value;

    #[test]
    fn lowercases_and_strips_diacritics() {
        assert_eq!(get_clean_value("Amélie"), "amelie");
        assert_eq!(get_clean_value("Motörhead"), "motorhead");
    }

    /// The values below are read verbatim out of a real Jellyfin 10.11.8
    /// database — punctuation survives normalization.
    #[test]
    fn punctuation_is_kept_exactly_as_jellyfin_stores_it() {
        assert_eq!(
            get_clean_value("Spider-Man: Across the Spider-Verse"),
            "spider-man: across the spider-verse"
        );
        assert_eq!(get_clean_value("H. Jon Benjamin"), "h. jon benjamin");
        assert_eq!(
            get_clean_value("Warner Bros. Pictures"),
            "warner bros. pictures"
        );
    }

    #[test]
    fn blank_input_is_returned_unchanged() {
        assert_eq!(get_clean_value(""), "");
        assert_eq!(get_clean_value("   "), "   ");
    }

    #[test]
    fn digits_are_kept() {
        assert_eq!(
            get_clean_value("2001: A Space Odyssey"),
            "2001: a space odyssey"
        );
    }
}
