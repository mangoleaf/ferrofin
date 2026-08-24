//! The alphanumeric sort key Jellyfin derives from an item's name.
//!
//! Port of `BaseItem.CreateSortName` + `BaseItem.ModifySortChunks`
//! (`MediaBrowser.Controller/Entities/BaseItem.cs`) under the default server
//! configuration. It lives here rather than in the scanner because the guide
//! (Live TV channels and programmes, which never pass through a scan) needs the
//! same key: one definition, one behaviour.
//!
//! The key is load-bearing beyond display order — clients build their play queue
//! from `SortName`, so an episode ordering that differs from Jellyfin's makes
//! "next episode"/autoplay pick the wrong item.

use crate::string_extensions::remove_diacritics;

/// The words `ServerConfiguration.SortRemoveWords` carries by default.
const SORT_REMOVE_WORDS: [&str; 3] = ["the", "a", "an"];

/// The characters `ServerConfiguration.SortRemoveCharacters` drops by default.
const SORT_REMOVE_CHARACTERS: [char; 6] = [',', '&', '-', '{', '}', '\''];

/// The characters `ServerConfiguration.SortReplaceCharacters` turns into a space.
const SORT_REPLACE_CHARACTERS: [char; 3] = ['.', '+', '%'];

/// How wide `ModifySortChunks` left-pads a run of digits, so "2" sorts before
/// "10" as text.
const SORT_CHUNK_WIDTH: usize = 10;

/// The sort key for `name`.
///
/// Port of `CreateSortName` with `EnableAlphaNumericSorting` on (the default):
/// lower-cased and trimmed, each configured word removed at the start, in the
/// middle and at the end, then the remove/replace character passes, then
/// [`modify_sort_chunks`]. The word pass runs FIRST — doing it after the
/// character passes would keep an interior article ("take a bow"), which sorts
/// differently from Jellyfin.
#[must_use]
pub fn create_sort_name(name: &str) -> String {
    let mut sortable = name.trim().to_lowercase();
    for word in SORT_REMOVE_WORDS {
        // Remove from beginning if a space follows…
        let prefix = format!("{word} ");
        if let Some(rest) = sortable.strip_prefix(&prefix) {
            sortable = rest.to_owned();
        }
        // …from the middle if surrounded by spaces…
        sortable = sortable.replace(&format!(" {word} "), " ");
        // …and from the end if preceded by a space.
        let suffix = format!(" {word}");
        if let Some(rest) = sortable.strip_suffix(&suffix) {
            sortable = rest.to_owned();
        }
    }
    for ch in SORT_REMOVE_CHARACTERS {
        sortable = sortable.replace(ch, "");
    }
    for ch in SORT_REPLACE_CHARACTERS {
        sortable = sortable.replace(ch, " ");
    }
    modify_sort_chunks(&sortable)
}

/// Left-pads every run of digits to [`SORT_CHUNK_WIDTH`] and strips diacritics.
///
/// Port of `BaseItem.ModifySortChunks`. (C# `char.IsDigit` covers the Unicode
/// `Nd` category; ASCII digits cover every value a media name or guide feed
/// carries. The final ICU `Transliterated()` step — which romanizes a title
/// still non-ASCII after the strip, e.g. Cyrillic — is not ported: it would pull
/// an ICU dependency in for a sort key no Latin-script library needs.)
#[must_use]
pub fn modify_sort_chunks(name: &str) -> String {
    fn flush(chunk: &mut String, digit_chunk: bool, out: &mut String) {
        if digit_chunk && chunk.len() < SORT_CHUNK_WIDTH {
            for _ in 0..(SORT_CHUNK_WIDTH - chunk.len()) {
                out.push('0');
            }
        }
        out.push_str(chunk);
        chunk.clear();
    }
    let mut out = String::with_capacity(name.len() + SORT_CHUNK_WIDTH - 1);
    let mut chunk = String::new();
    let mut digit_chunk = false;
    for ch in name.chars() {
        let is_digit = ch.is_ascii_digit();
        if !chunk.is_empty() && is_digit != digit_chunk {
            flush(&mut chunk, digit_chunk, &mut out);
        }
        digit_chunk = is_digit;
        chunk.push(ch);
    }
    flush(&mut chunk, digit_chunk, &mut out);
    remove_diacritics(&out)
}

#[cfg(test)]
mod tests {
    use super::{create_sort_name, modify_sort_chunks};

    // The upstream xUnit oracle, transliterated verbatim
    // (`Jellyfin.Controller.Tests/Entities/BaseItemTests.cs`
    // `BaseItem_ModifySortChunks_Valid`).
    #[rstest::rstest]
    #[case("", "")]
    #[case("1", "0000000001")]
    #[case("t", "t")]
    #[case("test", "test")]
    #[case("test1", "test0000000001")]
    #[case("1test 2", "0000000001test 0000000002")]
    fn modify_sort_chunks_matches_the_upstream_oracle(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(modify_sort_chunks(input), expected);
    }

    #[test]
    fn digit_runs_pad_and_diacritics_are_stripped() {
        assert_eq!(
            create_sort_name("Movie 0001 (2020)"),
            "movie 0000000001 (0000002020)"
        );
        assert_eq!(create_sort_name("Se7en"), "se0000000007en");
        assert_eq!(create_sort_name("Café Größe"), "cafe grosse");
    }

    #[test]
    fn configured_words_go_from_the_start_middle_and_end() {
        assert_eq!(create_sort_name("The Matrix"), "matrix");
        assert_eq!(create_sort_name("An Education"), "education");
        // Interior and trailing articles too — the pass upstream runs before the
        // character passes, which an article-prefix-only version would miss.
        assert_eq!(create_sort_name("Take a Bow"), "take bow");
        assert_eq!(create_sort_name("Best of the Best"), "best of best");
        assert_eq!(create_sort_name("Kill the"), "kill");
        // Not a whole word, so it stays.
        assert_eq!(create_sort_name("Theodore"), "theodore");
    }

    #[test]
    fn characters_are_removed_or_replaced_per_the_default_configuration() {
        assert_eq!(create_sort_name("Mr. & Mrs-Smith"), "mr   mrssmith");
        assert_eq!(create_sort_name("{Braces} 'quoted'"), "braces quoted");
        // A replaced character becomes its own space; C# does not collapse runs.
        assert_eq!(create_sort_name("100% Wolf"), "0000000100  wolf");
    }

    #[test]
    fn a_blank_name_yields_a_blank_key() {
        assert_eq!(create_sort_name("   "), "");
    }
}
