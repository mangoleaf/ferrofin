//! Port of `DictionaryExtensions.cs` — first-non-blank lookup over several keys.

use std::collections::HashMap;
use std::hash::BuildHasher;

/// Gets a string from a string map, checking each key in order and stopping at
/// the first whose value is neither absent nor blank (whitespace-only).
///
/// Empty keys are skipped (mirroring the C# `string.IsNullOrEmpty(key)` guards
/// on the optional keys). Pass `&[]` extra keys, or up to four total as in the
/// original `key1..key4` signature.
#[must_use]
pub fn get_first_not_null_nor_white_space_value<'a, S: BuildHasher>(
    dictionary: &'a HashMap<String, String, S>,
    keys: &[&str],
) -> Option<&'a str> {
    for (i, key) in keys.iter().enumerate() {
        // The first key is always checked; subsequent (optional) keys are
        // skipped when empty, matching the C# overload defaults.
        if i > 0 && key.is_empty() {
            continue;
        }

        if let Some(val) = dictionary.get(*key)
            && !val.trim().is_empty()
        {
            return Some(val.as_str());
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn returns_first_non_blank() {
        let d = map(&[("a", "  "), ("b", "value")]);
        assert_eq!(
            Some("value"),
            get_first_not_null_nor_white_space_value(&d, &["a", "b"])
        );
    }

    #[test]
    fn returns_none_when_all_blank_or_missing() {
        let d = map(&[("a", "   ")]);
        assert_eq!(
            None,
            get_first_not_null_nor_white_space_value(&d, &["a", "b"])
        );
    }

    #[test]
    fn skips_empty_optional_keys() {
        let d = map(&[("c", "third")]);
        assert_eq!(
            Some("third"),
            get_first_not_null_nor_white_space_value(&d, &["a", "", "c"])
        );
    }
}
