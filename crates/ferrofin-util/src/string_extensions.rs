//! Port of `StringExtensions.cs` — string helpers for counting, splitting on a
//! needle, diacritic removal/detection, transliteration and normalization.
//!
//! C# used ICU4N + the `Diacritics.Extensions` library. Here diacritic removal
//! is done via Unicode NFD decomposition + stripping of combining marks, plus a
//! small explicit map for non-decomposable ligatures (e.g. `œ` → `oe`). This
//! matches the ported test oracle exactly.

use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// C# `@"[^\p{L}\p{N}\s]"` in `GetCleanValue` — anything that is not a Unicode
/// letter, number or whitespace.
static NOT_LETTER_DIGIT_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}\s]").expect("valid regex"));

/// C# `@"\s+"` in `GetCleanValue` — a run of whitespace to collapse.
static WHITESPACE_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("valid regex"));

/// Environment variable name that (in upstream Jellyfin) overrides the ICU
/// transliterator rule id. Surfaced here as a config value for parity.
pub const TRANSLITERATOR_ID_ENV: &str = "JELLYFIN_TRANSLITERATOR_ID";

/// Default ICU transliterator rule id used when the environment override is
/// unset. Retained as a documented config constant even though the Rust port
/// approximates transliteration with ASCII folding.
pub const DEFAULT_TRANSLITERATOR_ID: &str =
    "Any-Latin; Latin-Ascii; Lower; NFD; [:Nonspacing Mark:] Remove; [:Punctuation:] Remove;";

/// Non-decomposable ligatures / letters that NFD does not split into ASCII, but
/// which the diacritic oracle expects to be folded.
fn ligature_map() -> &'static HashMap<char, &'static str> {
    static MAP: OnceLock<HashMap<char, &'static str>> = OnceLock::new();
    MAP.get_or_init(|| {
        let mut m = HashMap::new();
        m.insert('œ', "oe");
        m.insert('Œ', "OE");
        m.insert('æ', "ae");
        m.insert('Æ', "AE");
        m.insert('ø', "o");
        m.insert('Ø', "O");
        m.insert('ł', "l");
        m.insert('Ł', "L");
        // NOT `ð`/`þ`: Jellyfin's `Diacritics.Extensions` leaves them alone, and
        // a real 10.11.8 library proves it — it stores `Person-Gisli Orn
        // Garðarsson` and `Person-Þorsteinn Bachmann`, folding every *other*
        // accent in the same name. Folding them here made those people
        // unfindable by name on an adopted database, which is the exact bug
        // this column exists to avoid. (`ø`, `æ`, `œ` and `ł` ARE folded there
        // — same library, same rows. `ß` is unverified either way: no name in
        // the library carries one and upstream's own test suite has no case.)
        m.insert('ß', "ss");
        m
    })
}

/// Strips the non-conforming Unicode replacement character (`U+FFFD`).
///
/// The C# regex also matched lone UTF-16 surrogates, but those cannot exist in
/// a Rust `String` (they are always valid UTF-8), so only the `U+FFFD` branch is
/// observable here.
fn strip_non_conforming_unicode(text: &str) -> String {
    text.chars().filter(|&c| c != '\u{FFFD}').collect()
}

fn contains_non_conforming_unicode(text: &str) -> bool {
    text.chars().any(|c| c == '\u{FFFD}')
}

/// Removes the diacritics characters from the string.
///
/// Non-conforming Unicode (`U+FFFD`) is stripped first, then the string is
/// NFD-decomposed, combining marks are removed, and remaining non-decomposable
/// ligatures are folded.
#[must_use]
pub fn remove_diacritics(text: &str) -> String {
    // Every step of the fold is identity on ASCII: only U+FFFD is stripped,
    // NFD/NFC leave ASCII alone, and no nonspacing mark or ligature key is ASCII.
    if text.is_ascii() {
        return text.to_owned();
    }
    fold_diacritics(text)
}

/// The full diacritic fold behind [`remove_diacritics`], without its ASCII
/// fast path.
fn fold_diacritics(text: &str) -> String {
    let cleaned = strip_non_conforming_unicode(text);
    let map = ligature_map();
    let mut decomposed = String::with_capacity(cleaned.len());
    for ch in cleaned.nfd() {
        // NFD combining marks are the Unicode "Mark, Nonspacing" (Mn) class;
        // in the Basic range they live in U+0300..=U+036F (and other blocks).
        if is_nonspacing_mark(ch) {
            continue;
        }
        if let Some(replacement) = map.get(&ch) {
            decomposed.push_str(replacement);
        } else {
            decomposed.push(ch);
        }
    }

    // Re-compose (NFC) so precomposed code points that carry no diacritics
    // (e.g. Hangul syllables, which NFD splits into jamo) are restored to their
    // original single-code-point form. Anything whose combining mark was removed
    // stays base-only, since the mark is gone before recomposition.
    decomposed.nfc().collect()
}

/// Checks whether the specified string has diacritics in it.
#[must_use]
pub fn has_diacritics(text: &str) -> bool {
    remove_diacritics(text) != *text || contains_non_conforming_unicode(text)
}

/// Returns whether a code point is a Unicode nonspacing combining mark.
fn is_nonspacing_mark(c: char) -> bool {
    matches!(c as u32,
        0x0300..=0x036F | // Combining Diacritical Marks
        0x0483..=0x0489 |
        0x0591..=0x05BD | 0x05BF | 0x05C1..=0x05C2 | 0x05C4..=0x05C5 | 0x05C7 |
        0x0610..=0x061A | 0x064B..=0x065F | 0x0670 |
        0x06D6..=0x06DC | 0x06DF..=0x06E4 | 0x06E7..=0x06E8 | 0x06EA..=0x06ED |
        0x0711 | 0x0730..=0x074A |
        0x1AB0..=0x1AFF |
        0x1DC0..=0x1DFF | // Combining Diacritical Marks Supplement
        0x20D0..=0x20FF) // Combining Diacritical Marks for Symbols
}

/// Case-insensitive `String.Replace` of every occurrence of `from` with `to`.
///
/// Mirrors C# `string.Replace(old, new, StringComparison.OrdinalIgnoreCase)`.
/// Empty search or replacement strings leave the input unchanged.
#[must_use]
pub fn replace_ignore_ascii_case(haystack: &str, from: &str, to: &str) -> String {
    if from.is_empty() || to.is_empty() {
        return haystack.to_owned();
    }
    let lower_hay = haystack.to_ascii_lowercase();
    let lower_from = from.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut cursor = 0;
    while let Some(rel) = lower_hay[cursor..].find(&lower_from) {
        let start = cursor + rel;
        out.push_str(&haystack[cursor..start]);
        out.push_str(to);
        cursor = start + from.len();
    }
    out.push_str(&haystack[cursor..]);
    out
}

/// Counts the number of occurrences of `needle` in `value`.
///
/// C# operated on a `ReadOnlySpan<char>` (UTF-16 code units); this operates on
/// `char`s (Unicode scalar values), which is the natural Rust equivalent and
/// agrees on every ported test row.
#[must_use]
pub fn count(value: &str, needle: char) -> usize {
    value.chars().filter(|&c| c == needle).count()
}

/// Returns the part on the left of the `needle` (first occurrence).
///
/// Returns the whole string if the needle is absent, and an empty string if the
/// input is empty.
#[must_use]
pub fn left_part(haystack: &str, needle: char) -> &str {
    if haystack.is_empty() {
        return "";
    }

    match haystack.find(needle) {
        None => haystack,
        Some(pos) => &haystack[..pos],
    }
}

/// Returns the part on the right of the `needle` (last occurrence).
///
/// Returns the whole string if the needle is absent, and an empty string if the
/// needle is the final character or the input is empty.
#[must_use]
pub fn right_part(haystack: &str, needle: char) -> &str {
    if haystack.is_empty() {
        return "";
    }

    match haystack.rfind(needle) {
        None => haystack,
        Some(pos) => {
            let after = pos + needle.len_utf8();
            if after >= haystack.len() {
                ""
            } else {
                &haystack[after..]
            }
        }
    }
}

/// Returns a transliterated string which only contains ASCII characters.
///
/// The ICU rule-driven transliteration has no direct Rust analog; this
/// approximates it with diacritic folding, matching the documented intent.
#[must_use]
pub fn transliterated(text: &str) -> String {
    remove_diacritics(text)
}

/// Ensures all strings are non-null and trimmed of leading and trailing blanks.
pub fn trimmed<I, S>(values: I) -> impl Iterator<Item = String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    values.into_iter().map(|s| s.as_ref().trim().to_owned())
}

/// Truncates a string at the first null character (`'\0'`).
///
/// Returns the substring up to (but not including) the first null character, or
/// the original string if no null character is present.
#[must_use]
pub fn truncate_at_null(text: &str) -> &str {
    if text.is_empty() {
        text
    } else {
        left_part(text, '\0')
    }
}

/// Normalizes a string for comparison, the way the item repository's stored
/// `CleanName` / `CleanValue` columns are written.
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
/// Every character that is not a Unicode letter (`\p{L}`), digit (`\p{N}`) or
/// whitespace becomes one space, whitespace runs collapse to a single space,
/// and the result is trimmed — so `"H. Jon Benjamin"` stores as
/// `"h jon benjamin"` and `"Dune: Part Two"` as `"dune part two"`. 10.11.8
/// stopped after the diacritic fold and the lower-casing (its
/// `BaseItemRepository.GetCleanValue`); 12.0's
/// `RefreshCleanNamesAndValues` routine rewrites every stored column to this
/// form, and Ferrofin's `repair_clean_values` does the same once per database.
///
/// The character classes are the regex ones on purpose: `\p{L}` is the Unicode
/// *Letter* category, which excludes combining marks (`Mn`/`Mc`) that
/// [`char::is_alphabetic`] would keep — a Devanagari vowel sign is replaced by
/// a space here exactly as .NET does it.
///
/// Returns the original string unchanged if it is empty or all whitespace.
#[must_use]
pub fn get_clean_value(value: &str) -> String {
    if value.trim().is_empty() {
        return value.to_owned();
    }
    let cleaned = lower_invariant(&remove_diacritics(value));
    let cleaned = NOT_LETTER_DIGIT_SPACE.replace_all(&cleaned, " ");
    WHITESPACE_RUN.replace_all(&cleaned, " ").trim().to_owned()
}

/// `string.Equals(a, b, StringComparison.OrdinalIgnoreCase)`.
///
/// Not `a.to_lowercase() == b.to_lowercase()`: .NET's ordinal-ignore-case
/// comparison applies the **simple** invariant uppercase mapping to each
/// character, so a mapping that changes length (`ß` → `SS`) does not apply and
/// characters that lowercase onto an ASCII letter under full Unicode rules —
/// the Kelvin sign `K` (U+212A) lowercases to `k` in Rust — stay distinct. Lower
/// casing both sides folds those together and would match strings .NET does not.
/// Nor is it [`upper_invariant`]: ordinal casing keeps the long s `ſ`, which
/// `ToUpperInvariant` maps to `S`.
#[must_use]
pub fn equals_ordinal_ignore_case(left: &str, right: &str) -> bool {
    left.chars()
        .map(to_upper_ordinal)
        .eq(right.chars().map(to_upper_ordinal))
}

/// `StringComparer.OrdinalIgnoreCase.Compare(a, b)`: the same per-character
/// uppercase mapping as [`equals_ordinal_ignore_case`], then an ordinal
/// comparison of UTF-16 code units (so a supplementary-plane character sorts by
/// its surrogate pair, as in .NET, not by its scalar value).
#[must_use]
pub fn compare_ordinal_ignore_case(left: &str, right: &str) -> std::cmp::Ordering {
    let utf16 = |text: &str| -> Vec<u16> {
        text.chars()
            .map(to_upper_ordinal)
            .collect::<String>()
            .encode_utf16()
            .collect()
    };
    utf16(left).cmp(&utf16(right))
}

/// The key of a `StringComparer.OrdinalIgnoreCase` dictionary or set: two
/// strings get the same key exactly when [`equals_ordinal_ignore_case`] calls
/// them equal.
#[must_use]
pub fn ordinal_ignore_case_key(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_uppercase();
    }
    text.chars().map(to_upper_ordinal).collect()
}

/// .NET invariant uppercase for a string, using ICU's simple mappings.
///
/// Not an `OrdinalIgnoreCase` key: that is [`ordinal_ignore_case_key`].
#[must_use]
pub fn upper_invariant(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_uppercase();
    }
    text.chars().map(to_upper_invariant).collect()
}

/// .NET invariant lowercase for a string, using ICU's simple mappings.
#[must_use]
pub fn lower_invariant(text: &str) -> String {
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    let mapper = icu_casemap::CaseMapper::new();
    text.chars()
        // .NET ChangeCaseInvariant preserves capital dotted I.
        .map(|c| {
            if c == '\u{0130}' || NEWER_UNICODE_CASE_PAIRS.binary_search(&c).is_ok() {
                c
            } else {
                mapper.simple_lowercase(c)
            }
        })
        .collect()
}

/// One character as `OrdinalIgnoreCase` folds it: the invariant simple
/// uppercase, except the long s `ſ` (U+017F), which the runtime's ordinal
/// casing table keeps as itself (measured on .NET 10: the only character where
/// it differs from `ToUpperInvariant`).
fn to_upper_ordinal(c: char) -> char {
    if c == '\u{017f}' {
        c
    } else {
        to_upper_invariant(c)
    }
}

fn to_upper_invariant(c: char) -> char {
    // .NET ChangeCaseInvariant preserves dotless i, unlike Unicode's default.
    if c == '\u{0131}' || NEWER_UNICODE_CASE_PAIRS.binary_search(&c).is_ok() {
        c
    } else {
        icu_casemap::CaseMapper::new().simple_uppercase(c)
    }
}

/// The 20 case pairs Unicode 18 added, which ICU4X 2.4 maps and .NET's invariant
/// casing does not: .NET cases through the system ICU, and every ICU before 79
/// (Jellyfin's runtimes, and the ICU 78.3 behind `tests/data/dotnet-invariant-casing.tsv`)
/// predates them. Sorted for `binary_search`.
// shortcut: covers Unicode 18's new pairs only (a later ICU4X adds more, and the casing oracle test
// fails on the first one it meets), revisit at the dependency freeze sweep (gl-1370) or when Jellyfin's runtime ICU
// reaches 79. Dropping an entry changes stored keys built with these functions
// (`Users.NormalizedUsername`, CleanName, SortName), so it needs a backfill migration.
const NEWER_UNICODE_CASE_PAIRS: [char; 40] = [
    '\u{0277}',
    '\u{027c}',
    '\u{a7dd}',
    '\u{a7e2}',
    '\u{ab4b}',
    '\u{ab4c}',
    '\u{ab6c}',
    '\u{ab6d}',
    '\u{1df40}',
    '\u{1df41}',
    '\u{1df48}',
    '\u{1df49}',
    '\u{1df4a}',
    '\u{1df4b}',
    '\u{1df4d}',
    '\u{1df4e}',
    '\u{1df51}',
    '\u{1df52}',
    '\u{1df68}',
    '\u{1df69}',
    '\u{1df6a}',
    '\u{1df6b}',
    '\u{1df6c}',
    '\u{1df6d}',
    '\u{1df6e}',
    '\u{1df6f}',
    '\u{1df72}',
    '\u{1df73}',
    '\u{1df74}',
    '\u{1df75}',
    '\u{1df76}',
    '\u{1df77}',
    '\u{1df78}',
    '\u{1df79}',
    '\u{1df7a}',
    '\u{1df7b}',
    '\u{1df7c}',
    '\u{1df7d}',
    '\u{1df7e}',
    '\u{1df7f}',
];

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    #[test]
    fn invariant_casing_matches_dotnet_for_every_unicode_scalar() {
        let mappings: std::collections::HashMap<u32, (u32, u32)> =
            include_str!("../tests/data/dotnet-invariant-casing.tsv")
                .lines()
                .filter(|line| !line.starts_with('#'))
                .map(|line| {
                    let values: Vec<u32> = line
                        .split_whitespace()
                        .map(|n| u32::from_str_radix(n, 16).unwrap())
                        .collect();
                    (values[0], (values[1], values[2]))
                })
                .collect();
        for scalar in 0..=0x0010_ffff {
            let Some(c) = char::from_u32(scalar) else {
                continue;
            };
            let (upper, lower) = mappings.get(&scalar).copied().unwrap_or((scalar, scalar));
            let text = c.to_string();
            assert_eq!(
                upper_invariant(&text),
                char::from_u32(upper).unwrap().to_string(),
                "uppercase U+{scalar:04X}"
            );
            assert_eq!(
                lower_invariant(&text),
                char::from_u32(lower).unwrap().to_string(),
                "lowercase U+{scalar:04X}"
            );
        }
    }

    // .NET invariant casing: expansions, contextual casing and Unicode
    // normalization must not be introduced by a library upgrade.
    #[rstest]
    #[case("alice", "ALICE", "alice")]
    #[case("münchen", "MÜNCHEN", "münchen")]
    #[case("Ñoño", "ÑOÑO", "ñoño")]
    #[case("iIİı", "IIİı", "iiİı")]
    #[case("straßeẞ", "STRAßEẞ", "straßeß")]
    #[case("ﬃ", "ﬃ", "ﬃ")]
    #[case("σςΣ", "ΣΣΣ", "σςσ")]
    #[case("ᾀ", "ᾈ", "ᾀ")]
    #[case("Привет", "ПРИВЕТ", "привет")]
    #[case("աբգ", "ԱԲԳ", "աբգ")]
    #[case("აბგ", "ᲐᲑᲒ", "აბგ")]
    #[case("𐐨𐐀", "𐐀𐐀", "𐐨𐐨")]
    #[case("é e\u{301}", "É E\u{301}", "é e\u{301}")]
    #[case("aа", "AА", "aа")]
    #[case("中文🙂", "中文🙂", "中文🙂")]
    #[case("", "", "")]
    fn invariant_casing(#[case] input: &str, #[case] upper: &str, #[case] lower: &str) {
        assert_eq!(upper_invariant(input), upper);
        assert_eq!(lower_invariant(input), lower);
        assert_eq!(upper_invariant(upper), upper);
        assert_eq!(lower_invariant(lower), lower);
    }

    #[rstest]
    #[case("", "")] // Identity edge-case (no diacritics)
    #[case("Indiana Jones", "Indiana Jones")] // Identity (no diacritics)
    #[case("a\u{FFFD}b", "ab")] // Invalid UTF-16 char stripping (lone surrogate -> U+FFFD in Rust)
    #[case("åäö", "aao")] // Issue #7484
    #[case("Jön", "Jon")] // Issue #7484
    #[case("Jönssonligan", "Jonssonligan")] // Issue #7484
    #[case("Kieślowski", "Kieslowski")] // Issue #7450
    #[case("Cidadão Kane", "Cidadao Kane")] // Issue #7560
    #[case("운명처럼 널 사랑해", "운명처럼 널 사랑해")] // Issue #6393 (Korean language support)
    #[case("애타는 로맨스", "애타는 로맨스")] // Issue #6393
    #[case("Le cœur a ses raisons", "Le coeur a ses raisons")] // Issue #8893
    #[case("Béla Tarr", "Bela Tarr")] // Issue #8893
    fn remove_diacritics_valid_input_corrects(#[case] input: &str, #[case] expected: &str) {
        let result = remove_diacritics(input);
        assert_eq!(expected, result);
    }

    /// The ASCII fast path returns exactly what the full fold would: the fold
    /// leaves every ASCII char alone, all of them together, and the ASCII on
    /// both sides of a non-ASCII run (which takes the full fold).
    #[test]
    fn remove_diacritics_ascii_fast_path_matches_full_fold() {
        let all_ascii: String = (0u8..=127).map(char::from).collect();
        for input in all_ascii
            .chars()
            .map(String::from)
            .chain([all_ascii.clone()])
        {
            assert_eq!(fold_diacritics(&input), input, "{input:?}");
            assert_eq!(remove_diacritics(&input), input, "{input:?}");
        }
        for mixed in ["é", "e\u{301}", "\u{301}", "œ", "Ø", "ß", "\u{FFFD}", "운"] {
            let folded = fold_diacritics(mixed);
            let input = format!("{all_ascii}{mixed}{all_ascii}");
            assert_eq!(
                remove_diacritics(&input),
                format!("{all_ascii}{folded}{all_ascii}"),
                "{mixed:?}"
            );
        }
    }

    #[rstest]
    #[case("", false)] // Identity edge-case (no diacritics)
    #[case("Indiana Jones", false)] // Identity (no diacritics)
    #[case("a\u{FFFD}b", true)] // Invalid UTF-16 char stripping
    #[case("åäö", true)] // Issue #7484
    #[case("Jön", true)] // Issue #7484
    #[case("Jönssonligan", true)] // Issue #7484
    #[case("Kieślowski", true)] // Issue #7450
    #[case("Cidadão Kane", true)] // Issue #7560
    #[case("운명처럼 널 사랑해", false)] // Issue #6393 (Korean language support)
    #[case("애타는 로맨스", false)] // Issue #6393
    #[case("Le cœur a ses raisons", true)] // Issue #8893
    #[case("Béla Tarr", true)] // Issue #8893
    fn has_diacritics_valid_input_corrects(#[case] input: &str, #[case] expected: bool) {
        let result = has_diacritics(input);
        assert_eq!(expected, result);
    }

    #[rstest]
    #[case("", '_', 0)]
    #[case("___", '_', 3)]
    #[case("test\x00", '\x00', 1)]
    #[case("Imdb=tt0119567|Tmdb=330|TmdbCollection=328", '|', 2)]
    fn read_only_span_count_success(
        #[case] str: &str,
        #[case] needle: char,
        #[case] count_: usize,
    ) {
        assert_eq!(count_, count(str, needle));
    }

    #[rstest]
    #[case("", 'q', "")]
    #[case("Banana split", ' ', "Banana")]
    #[case("Banana split", 'q', "Banana split")]
    #[case("Banana split 2", ' ', "Banana")]
    fn left_part_valid_args_char_needle_correct(
        #[case] str: &str,
        #[case] needle: char,
        #[case] expected: &str,
    ) {
        let result = left_part(str, needle);
        assert_eq!(expected, result);
    }

    #[rstest]
    #[case("", 'q', "")]
    #[case("Banana split", ' ', "split")]
    #[case("Banana split", 'q', "Banana split")]
    #[case("Banana split.", '.', "")]
    #[case("Banana split 2", ' ', "2")]
    fn right_part_valid_args_char_needle_correct(
        #[case] str: &str,
        #[case] needle: char,
        #[case] expected: &str,
    ) {
        let result = right_part(str, needle);
        assert_eq!(expected, result);
    }

    // --- Coverage top-up: Rust-native behavior for public helpers the C# suite
    // did not exercise directly (transliterated / trimmed / truncate_at_null /
    // get_clean_value and its regex helpers).

    #[rstest]
    #[case("Béla Tarr", "Bela Tarr")] // diacritic folding, same oracle as remove_diacritics
    #[case("Indiana Jones", "Indiana Jones")] // identity
    fn transliterated_folds_diacritics(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(expected, transliterated(input));
    }

    #[test]
    fn trimmed_strips_leading_and_trailing_blanks() {
        let out: Vec<String> = trimmed(vec!["  a ", "\tb\n", "c", "   "]).collect();
        assert_eq!(vec!["a", "b", "c", ""], out);
    }

    #[rstest]
    #[case("", "")] // empty stays empty
    #[case("no-null", "no-null")] // no null -> unchanged
    #[case("keep\0drop", "keep")] // truncates at first null
    #[case("\0after", "")] // leading null -> empty
    fn truncate_at_null_cuts_at_first_null(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(expected, truncate_at_null(input));
    }

    /// Folded and unfolded characters, taken from a real Jellyfin 10.11.8
    /// library: every expected value here is a `PresentationUniqueKey` or
    /// `CleanName` that database actually stores.
    #[rstest]
    #[case("Gísli Örn Garðarsson", "Gisli Orn Garðarsson")] // eth survives
    #[case("Þorsteinn Bachmann", "Þorsteinn Bachmann")] // thorn survives
    #[case("Árni Þór Lárusson", "Arni Þor Larusson")] // …while its neighbours fold
    #[case("Pilou Asbæk", "Pilou Asbaek")]
    #[case("Roland Møller", "Roland Moller")]
    #[case("Kasia Kołeczek", "Kasia Koleczek")]
    fn remove_diacritics_matches_what_jellyfin_stored(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(expected, remove_diacritics(input));
    }

    // Jellyfin 12.0 `GetCleanValue`: fold, lowercase, punctuation → space,
    // collapse, trim.
    #[rstest]
    #[case("", "")] // empty passes through unchanged
    #[case("   ", "   ")] // all-whitespace passes through unchanged
    #[case("dune: part two", "dune part two")]
    #[case("weird: the al yankovic story", "weird the al yankovic story")]
    #[case("Mötley Crüe: Greatest Hits!", "motley crue greatest hits")]
    #[case("H. Jon Benjamin", "h jon benjamin")]
    #[case("Warner Bros. Pictures", "warner bros pictures")]
    #[case("ΟΣ", "οσ")] // simple casing: no final-sigma context
    #[case("𐐀", "𐐨")]
    #[case("Béla   Tarr!!", "bela tarr")] // fold, lowercase, punctuation to space, collapse
    #[case("A,B;C", "a b c")]
    #[case(
        "Spider-Man: Across the Spider-Verse",
        "spider man across the spider verse"
    )]
    #[case("Béla   Tarr!!", "bela tarr")] // runs collapse, trailing space trimmed
    #[case("A,B;C", "a b c")] // each punctuation mark is its own space, then collapsed
    #[case("2001: A Space Odyssey", "2001 a space odyssey")] // digits are letters' peers
    #[case("千と千尋の神隠し", "千と千尋の神隠し")] // CJK letters are \p{L}: untouched
    #[case("千と千尋の神隠し！", "千と千尋の神隠し")] // …a fullwidth mark is not
    #[case("Ａｂｃ", "ａｂｃ")] // fullwidth letters lowercase and survive
    #[case("  padded  ", "padded")]
    #[case("tab\tand\nnewline", "tab and newline")]
    // Upstream `SearchPunctuationTests.cs` (jellyfin/jellyfin PR 14879), the
    // C# expected values verbatim.
    #[case("Mr. Robot", "mr robot")]
    #[case("Spider-Man: Homecoming", "spider man homecoming")]
    #[case("Beyoncé — Live!", "beyonce live")]
    #[case("Hello, World!", "hello world")]
    #[case("(The) Good, the Bad & the Ugly", "the good the bad the ugly")]
    #[case("Wall-E", "wall e")]
    #[case("No. 1: The Beginning", "no 1 the beginning")]
    #[case("Café-au-lait", "cafe au lait")]
    #[case("Face/Off", "face off")]
    #[case("V/H/S", "v h s")]
    fn get_clean_value_normalizes(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(expected, get_clean_value(input));
    }

    #[test]
    fn ordinal_ignore_case_matches_dotnet() {
        assert!(equals_ordinal_ignore_case("Bookshelf", "BOOKSHELF"));
        assert!(equals_ordinal_ignore_case("", ""));
        assert!(equals_ordinal_ignore_case("ÉCLAIR", "éclair"));
        assert!(!equals_ordinal_ignore_case("Bookshelf", "Bookshelves"));
        assert!(!equals_ordinal_ignore_case("a", "b"));
        // The two cases a `to_lowercase()` comparison gets wrong: `ß` has no
        // single-character uppercase, and the Kelvin sign is not `k`.
        assert!(!equals_ordinal_ignore_case("straße", "STRASSE"));
        assert!(!equals_ordinal_ignore_case("\u{212a}elvin", "kelvin"));
        assert_eq!("\u{212a}".to_lowercase(), "k", "the trap this avoids");
    }

    /// `string.Equals(a, b, StringComparison.OrdinalIgnoreCase)` as .NET 10.0.14
    /// answers it, for the comparison and for the dictionary key.
    #[rstest]
    #[case("Disney", "DISNEY", true)]
    #[case("SOCIÉTÉ", "Société", true)]
    #[case("ö", "Ö", true)]
    #[case("ß", "SS", false)]
    #[case("straße", "STRASSE", false)]
    #[case("ß", "ẞ", false)]
    #[case("STRAßE", "straße", true)]
    #[case("\u{17f}", "S", false)] // the long s: ToUpperInvariant maps it to S, ordinal casing does not
    #[case("\u{17f}", "s", false)]
    #[case("\u{131}", "I", false)] // the dotless i keeps its case in both
    #[case("\u{212a}", "k", false)]
    #[case("\u{212a}", "K", false)]
    #[case("\u{1c5}", "\u{1c4}", true)]
    #[case("\u{1c5}", "\u{1c6}", true)]
    #[case("\u{10428}", "\u{10400}", true)] // a supplementary-plane letter (Deseret)
    #[case("ﬁ", "FI", false)]
    fn ordinal_ignore_case_key_matches_dotnet(
        #[case] left: &str,
        #[case] right: &str,
        #[case] equal: bool,
    ) {
        assert_eq!(equals_ordinal_ignore_case(left, right), equal);
        assert_eq!(
            ordinal_ignore_case_key(left) == ordinal_ignore_case_key(right),
            equal
        );
    }

    #[test]
    fn compare_ordinal_ignore_case_matches_dotnet() {
        use std::cmp::Ordering::{Equal, Greater, Less};
        assert_eq!(compare_ordinal_ignore_case("alien", "ALIEN"), Equal);
        assert_eq!(compare_ordinal_ignore_case("alien", "Blade"), Less);
        // Uppercased first: `_` (0x5F) sorts after `A` (0x41) but before `a`.
        assert_eq!(compare_ordinal_ignore_case("_x", "ax"), Greater);
        assert_eq!(compare_ordinal_ignore_case("", "a"), Less);
        // UTF-16 order: a surrogate (0xD83C…) sorts below U+FF21 `Ａ`.
        assert_eq!(compare_ordinal_ignore_case("\u{1F3AC}", "\u{FF21}"), Less);
        // The long s stays U+017F, so it sorts after every ASCII letter.
        assert_eq!(compare_ordinal_ignore_case("\u{17f}", "T"), Greater);
    }

    /// `ToUpperInvariant` is the simple mapping: one char in, one char out.
    #[rstest]
    #[case("", "")]
    #[case("alice", "ALICE")]
    #[case("Alice Smith", "ALICE SMITH")]
    #[case("münchen", "MÜNCHEN")]
    #[case("þór", "ÞÓR")]
    #[case("straße", "STRAßE")] // ß has no single-char uppercase → kept
    #[case("ﬁlm", "ﬁLM")] // the ﬁ ligature expands under the full mapping → kept
    #[case("ŉ", "ŉ")] // expands to ʼN under the full mapping → kept
    #[case("ǆ", "Ǆ")] // a digraph with a one-char uppercase
    #[case("Ωmega", "ΩMEGA")]
    fn upper_invariant_is_the_simple_mapping(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(upper_invariant(input), expected);
        // Idempotent, like the C#: normalising a stored key changes nothing.
        assert_eq!(upper_invariant(expected), expected);
    }
}
