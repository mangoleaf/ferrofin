//! Small text-normalization helpers shared by the item repository.
//!
//! Port of the `GetCleanValue` / `RemoveDiacritics` extension methods in
//! Jellyfin's `Jellyfin.Extensions.StringExtensions`. The item repository stores
//! a normalized `CleanName` / `CleanValue` alongside every display name so that
//! name, genre, tag, and artist lookups are diacritic- and punctuation-
//! insensitive; the query translator recomputes the same normalization on the
//! filter values so they compare against the stored clean columns.

/// Normalizes a display value into its stored "clean" form.
///
/// Mirrors C# `string.GetCleanValue()`:
/// 1. remove diacritics (é → e) and lowercase, then
/// 2. replace every character that is not a letter, digit, or whitespace with a
///    space, then
/// 3. collapse runs of whitespace to a single space and trim.
///
/// A blank or whitespace-only input is returned unchanged (matching the C#
/// `IsNullOrWhiteSpace` short-circuit).
///
/// The diacritic removal here folds the common Latin-1/Latin-Extended-A range
/// (the diacritics that appear in real library metadata); it is a pragmatic
/// subset of the full Unicode canonical decomposition the C# `Diacritics`
/// package performs, sufficient for the ASCII-folding the clean columns rely on.
#[must_use]
pub fn get_clean_value(value: &str) -> String {
    if value.trim().is_empty() {
        return value.to_owned();
    }

    let lowered_folded: String = value.chars().flat_map(fold_char).collect::<String>();
    let lowered = lowered_folded.to_lowercase();

    let mut out = String::with_capacity(lowered.len());
    let mut last_was_space = false;
    for ch in lowered.chars() {
        let keep = ch.is_alphabetic() || ch.is_numeric();
        if keep {
            out.push(ch);
            last_was_space = false;
        } else {
            // Whitespace and every other symbol collapse to a single space.
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        }
    }

    out.trim().to_owned()
}

/// Folds a single character to its diacritic-free base character(s).
///
/// Returns the base ASCII letters for the common accented Latin characters and
/// the identity mapping otherwise. A few characters (e.g. `æ`, `ß`) fold to two
/// characters, hence the iterator return type.
fn fold_char(ch: char) -> impl Iterator<Item = char> {
    let mapped: &[char] = match ch {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' => &['a'],
        'æ' | 'Æ' => &['a', 'e'],
        'ç' | 'Ç' => &['c'],
        'è' | 'é' | 'ê' | 'ë' | 'È' | 'É' | 'Ê' | 'Ë' => &['e'],
        'ì' | 'í' | 'î' | 'ï' | 'Ì' | 'Í' | 'Î' | 'Ï' => &['i'],
        'ñ' | 'Ñ' => &['n'],
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' => &['o'],
        'ù' | 'ú' | 'û' | 'ü' | 'Ù' | 'Ú' | 'Û' | 'Ü' => &['u'],
        'ý' | 'ÿ' | 'Ý' => &['y'],
        'ß' => &['s', 's'],
        other => return CharFold::One(std::iter::once(other)),
    };
    CharFold::Many(mapped.iter().copied())
}

/// Iterator over the folded characters for a single input character.
///
/// A hand-rolled enum keeps [`fold_char`] returning one concrete type without
/// boxing.
enum CharFold {
    One(std::iter::Once<char>),
    Many(std::iter::Copied<std::slice::Iter<'static, char>>),
}

impl Iterator for CharFold {
    type Item = char;

    fn next(&mut self) -> Option<char> {
        match self {
            CharFold::One(it) => it.next(),
            CharFold::Many(it) => it.next(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::get_clean_value;

    #[test]
    fn lowercases_and_strips_diacritics() {
        assert_eq!(get_clean_value("Amélie"), "amelie");
        assert_eq!(get_clean_value("Motörhead"), "motorhead");
    }

    #[test]
    fn punctuation_becomes_collapsed_spaces() {
        assert_eq!(
            get_clean_value("Spider-Man: No Way Home"),
            "spider man no way home"
        );
        assert_eq!(get_clean_value("A.I."), "a i");
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
            "2001 a space odyssey"
        );
    }
}
