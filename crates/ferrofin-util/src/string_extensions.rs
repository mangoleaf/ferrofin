//! Port of `StringExtensions.cs` — string helpers for counting, splitting on a
//! needle, diacritic removal/detection, transliteration and normalization.
//!
//! C# used ICU4N + the `Diacritics` package. Diacritic removal is that
//! package's character table ([`remove_diacritics`]); transliteration runs
//! ICU4X's CLDR transforms (see [`transliterated`]).
//!
//! [`zero_padded`] is not from that file: it is .NET's zero-placeholder custom
//! format of an integer (`ToString("000")`).

use std::collections::HashMap;
use std::sync::{LazyLock, OnceLock};

use regex::Regex;
use unicode_normalization::UnicodeNormalization;

/// C# `@"[^\p{L}\p{N}\s]"` in `GetCleanValue` — anything that is not a Unicode
/// letter, number or whitespace. .NET matches it against UTF-16 units, so a
/// character outside the Basic Multilingual Plane, two surrogates of category
/// `Cs`, never counts as a letter or a number.
static NOT_LETTER_DIGIT_SPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}\s]|[\x{10000}-\x{10FFFF}]").expect("valid regex"));

/// [`NOT_LETTER_DIGIT_SPACE`] as builds before gl-1908 matched it, by code
/// point, for [`get_clean_value_nfd`].
static NOT_LETTER_DIGIT_SPACE_BY_CODE_POINT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^\p{L}\p{N}\s]").expect("valid regex"));

/// C# `@"\s+"` in `GetCleanValue` — a run of whitespace to collapse.
static WHITESPACE_RUN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+").expect("valid regex"));

/// Environment variable name that, in upstream Jellyfin, replaces the ICU
/// transliterator rule id. Ferrofin does not read it yet: [`transliterated`]
/// always runs [`DEFAULT_TRANSLITERATOR_ID`] (gl-327).
pub const TRANSLITERATOR_ID_ENV: &str = "JELLYFIN_TRANSLITERATOR_ID";

/// Upstream's default ICU transliterator rule id, the rule [`transliterated`]
/// runs.
pub const DEFAULT_TRANSLITERATOR_ID: &str =
    "Any-Latin; Latin-Ascii; Lower; NFD; [:Nonspacing Mark:] Remove; [:Punctuation:] Remove;";

/// The ligatures and letters NFD does not split that [`remove_diacritics_nfd`]
/// folds.
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
/// Port of upstream's `RemoveDiacritics`
/// (`src/Jellyfin.Extensions/StringExtensions.cs:42-44`): non-conforming
/// Unicode (`U+FFFD`) is stripped, then every character the `Diacritics`
/// package maps is replaced by its base ([`diacritic_base`]). The package
/// looks each UTF-16 unit up in a table and normalises nothing, so a
/// combining mark stays where it is (`Saldan\u{303}a` is unchanged; only
/// U+0327, the combining cedilla, is in the table), and so do the precomposed
/// letters the table lacks (`ĉ`, pinyin `ǒ`, Greek polytonic, Hebrew
/// presentation forms).
#[must_use]
pub fn remove_diacritics(text: &str) -> String {
    // No key of the table is ASCII, and neither is U+FFFD.
    if text.is_ascii() {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len());
    for c in text.chars().filter(|&c| c != '\u{FFFD}') {
        match diacritic_base(c) {
            Some(base) => out.push_str(base),
            None => out.push(c),
        }
    }
    out
}

/// The base the `Diacritics` package replaces `c` with, or `None` when `c` is
/// not in its table ([`DIACRITICS`]).
fn diacritic_base(c: char) -> Option<&'static str> {
    DIACRITICS
        .binary_search_by_key(&c, |&(key, _)| key)
        .ok()
        .map(|at| DIACRITICS[at].1)
}

/// The table of `Diacritics` 4.1.8 as Jellyfin 12.4 ships it, read from the
/// package's own `DiacriticsMapper` (gl-1908), in code point order: the
/// package's 283 entries but `ß`, which maps to itself, each mapped to its
/// `MappingReplacement.Base` (the `Decompose` forms are opt-in, and Jellyfin
/// does not ask for them). The four with no base (`¡`, `¿`, U+0327 and `⸘`)
/// are removed. No key lies outside the Basic Multilingual Plane, so walking
/// `char`s finds what the package finds walking UTF-16 units.
static DIACRITICS: [(char, &str); 282] = [
    ('\u{00A1}', ""),         // ¡
    ('\u{00AA}', "a"),        // ª
    ('\u{00BA}', "o"),        // º
    ('\u{00BF}', ""),         // ¿
    ('\u{00C0}', "A"),        // À
    ('\u{00C1}', "A"),        // Á
    ('\u{00C2}', "A"),        // Â
    ('\u{00C3}', "A"),        // Ã
    ('\u{00C4}', "A"),        // Ä
    ('\u{00C5}', "A"),        // Å
    ('\u{00C7}', "C"),        // Ç
    ('\u{00C8}', "E"),        // È
    ('\u{00C9}', "E"),        // É
    ('\u{00CA}', "E"),        // Ê
    ('\u{00CB}', "E"),        // Ë
    ('\u{00CC}', "I"),        // Ì
    ('\u{00CD}', "I"),        // Í
    ('\u{00CE}', "I"),        // Î
    ('\u{00CF}', "I"),        // Ï
    ('\u{00D1}', "N"),        // Ñ
    ('\u{00D2}', "O"),        // Ò
    ('\u{00D3}', "O"),        // Ó
    ('\u{00D4}', "O"),        // Ô
    ('\u{00D5}', "O"),        // Õ
    ('\u{00D6}', "O"),        // Ö
    ('\u{00D8}', "O"),        // Ø
    ('\u{00D9}', "U"),        // Ù
    ('\u{00DA}', "U"),        // Ú
    ('\u{00DB}', "U"),        // Û
    ('\u{00DC}', "U"),        // Ü
    ('\u{00DD}', "Y"),        // Ý
    ('\u{00E0}', "a"),        // à
    ('\u{00E1}', "a"),        // á
    ('\u{00E2}', "a"),        // â
    ('\u{00E3}', "a"),        // ã
    ('\u{00E4}', "a"),        // ä
    ('\u{00E5}', "a"),        // å
    ('\u{00E6}', "ae"),       // æ
    ('\u{00E7}', "c"),        // ç
    ('\u{00E8}', "e"),        // è
    ('\u{00E9}', "e"),        // é
    ('\u{00EA}', "e"),        // ê
    ('\u{00EB}', "e"),        // ë
    ('\u{00EC}', "i"),        // ì
    ('\u{00ED}', "i"),        // í
    ('\u{00EE}', "i"),        // î
    ('\u{00EF}', "i"),        // ï
    ('\u{00F1}', "n"),        // ñ
    ('\u{00F2}', "o"),        // ò
    ('\u{00F3}', "o"),        // ó
    ('\u{00F4}', "o"),        // ô
    ('\u{00F5}', "o"),        // õ
    ('\u{00F6}', "o"),        // ö
    ('\u{00F8}', "o"),        // ø
    ('\u{00F9}', "u"),        // ù
    ('\u{00FA}', "u"),        // ú
    ('\u{00FB}', "u"),        // û
    ('\u{00FC}', "u"),        // ü
    ('\u{00FD}', "y"),        // ý
    ('\u{00FF}', "y"),        // ÿ
    ('\u{0100}', "A"),        // Ā
    ('\u{0101}', "a"),        // ā
    ('\u{0102}', "A"),        // Ă
    ('\u{0103}', "a"),        // ă
    ('\u{0104}', "A"),        // Ą
    ('\u{0105}', "a"),        // ą
    ('\u{0106}', "C"),        // Ć
    ('\u{0107}', "c"),        // ć
    ('\u{010C}', "C"),        // Č
    ('\u{010D}', "c"),        // č
    ('\u{010E}', "D"),        // Ď
    ('\u{010F}', "d"),        // ď
    ('\u{0110}', "D"),        // Đ
    ('\u{0111}', "d"),        // đ
    ('\u{0112}', "E"),        // Ē
    ('\u{0113}', "e"),        // ē
    ('\u{0116}', "E"),        // Ė
    ('\u{0117}', "e"),        // ė
    ('\u{0118}', "E"),        // Ę
    ('\u{0119}', "e"),        // ę
    ('\u{011A}', "E"),        // Ě
    ('\u{011B}', "e"),        // ě
    ('\u{011E}', "G"),        // Ğ
    ('\u{011F}', "g"),        // ğ
    ('\u{0122}', "G"),        // Ģ
    ('\u{0123}', "g"),        // ģ
    ('\u{0128}', "I"),        // Ĩ
    ('\u{0129}', "i"),        // ĩ
    ('\u{012A}', "I"),        // Ī
    ('\u{012B}', "i"),        // ī
    ('\u{012E}', "I"),        // Į
    ('\u{012F}', "i"),        // į
    ('\u{0130}', "I"),        // İ
    ('\u{0131}', "i"),        // ı
    ('\u{0136}', "K"),        // Ķ
    ('\u{0137}', "k"),        // ķ
    ('\u{0139}', "L"),        // Ĺ
    ('\u{013A}', "l"),        // ĺ
    ('\u{013B}', "L"),        // Ļ
    ('\u{013C}', "l"),        // ļ
    ('\u{013D}', "L"),        // Ľ
    ('\u{013E}', "l"),        // ľ
    ('\u{0141}', "L"),        // Ł
    ('\u{0142}', "l"),        // ł
    ('\u{0143}', "N"),        // Ń
    ('\u{0144}', "n"),        // ń
    ('\u{0145}', "N"),        // Ņ
    ('\u{0146}', "n"),        // ņ
    ('\u{0147}', "N"),        // Ň
    ('\u{0148}', "n"),        // ň
    ('\u{014C}', "O"),        // Ō
    ('\u{014D}', "o"),        // ō
    ('\u{0150}', "O"),        // Ő
    ('\u{0151}', "o"),        // ő
    ('\u{0153}', "oe"),       // œ
    ('\u{0154}', "R"),        // Ŕ
    ('\u{0155}', "r"),        // ŕ
    ('\u{0158}', "R"),        // Ř
    ('\u{0159}', "r"),        // ř
    ('\u{015A}', "S"),        // Ś
    ('\u{015B}', "s"),        // ś
    ('\u{015E}', "S"),        // Ş
    ('\u{015F}', "s"),        // ş
    ('\u{0160}', "S"),        // Š
    ('\u{0161}', "s"),        // š
    ('\u{0164}', "T"),        // Ť
    ('\u{0165}', "t"),        // ť
    ('\u{0168}', "U"),        // Ũ
    ('\u{0169}', "u"),        // ũ
    ('\u{016A}', "U"),        // Ū
    ('\u{016B}', "u"),        // ū
    ('\u{016E}', "U"),        // Ů
    ('\u{016F}', "u"),        // ů
    ('\u{0170}', "U"),        // Ű
    ('\u{0171}', "u"),        // ű
    ('\u{0178}', "Y"),        // Ÿ
    ('\u{0179}', "Z"),        // Ź
    ('\u{017A}', "z"),        // ź
    ('\u{017B}', "Z"),        // Ż
    ('\u{017C}', "z"),        // ż
    ('\u{017D}', "Z"),        // Ž
    ('\u{017E}', "z"),        // ž
    ('\u{018F}', "A"),        // Ə
    ('\u{01A0}', "O"),        // Ơ
    ('\u{01A1}', "o"),        // ơ
    ('\u{01AF}', "U"),        // Ư
    ('\u{01B0}', "u"),        // ư
    ('\u{0218}', "S"),        // Ș
    ('\u{0219}', "s"),        // ș
    ('\u{021A}', "T"),        // Ț
    ('\u{021B}', "t"),        // ț
    ('\u{0259}', "a"),        // ə
    ('\u{0327}', ""),         // combining cedilla
    ('\u{0386}', "\u{0391}"), // Ά -> Α
    ('\u{0388}', "\u{0395}"), // Έ -> Ε
    ('\u{0389}', "\u{0397}"), // Ή -> Η
    ('\u{038A}', "\u{0399}"), // Ί -> Ι
    ('\u{038C}', "\u{039F}"), // Ό -> Ο
    ('\u{038E}', "\u{03A5}"), // Ύ -> Υ
    ('\u{038F}', "\u{03A9}"), // Ώ -> Ω
    ('\u{0390}', "\u{03B9}"), // ΐ -> ι
    ('\u{03AA}', "\u{0399}"), // Ϊ -> Ι
    ('\u{03AB}', "\u{03A5}"), // Ϋ -> Υ
    ('\u{03AC}', "\u{03B1}"), // ά -> α
    ('\u{03AD}', "\u{03B5}"), // έ -> ε
    ('\u{03AE}', "\u{03B7}"), // ή -> η
    ('\u{03AF}', "\u{03B9}"), // ί -> ι
    ('\u{03B0}', "\u{03C5}"), // ΰ -> υ
    ('\u{03CA}', "\u{03B9}"), // ϊ -> ι
    ('\u{03CB}', "\u{03C5}"), // ϋ -> υ
    ('\u{03CC}', "\u{03BF}"), // ό -> ο
    ('\u{03CD}', "\u{03C5}"), // ύ -> υ
    ('\u{03CE}', "\u{03C9}"), // ώ -> ω
    ('\u{0401}', "\u{0415}"), // Ё -> Е
    ('\u{0407}', "\u{0406}"), // Ї -> І
    ('\u{040E}', "\u{0423}"), // Ў -> У
    ('\u{0419}', "\u{0418}"), // Й -> И
    ('\u{0439}', "\u{0438}"), // й -> и
    ('\u{0451}', "\u{0435}"), // ё -> е
    ('\u{0457}', "\u{0456}"), // ї -> і
    ('\u{045E}', "\u{0443}"), // ў -> у
    ('\u{0622}', "\u{0627}"), // آ -> ا
    ('\u{0623}', "\u{0627}"), // أ -> ا
    ('\u{0624}', "\u{0648}"), // ؤ -> و
    ('\u{0625}', "\u{0627}"), // إ -> ا
    ('\u{0626}', "\u{064A}"), // ئ -> ي
    ('\u{0649}', "\u{064A}"), // ى -> ي
    ('\u{0671}', "\u{0627}"), // ٱ -> ا
    ('\u{06C2}', "\u{06C1}"), // ۂ -> ہ
    ('\u{0B94}', "\u{0B92}"), // ஔ -> ஒ
    ('\u{1E9E}', "\u{00DF}"), // ẞ -> ß
    ('\u{1EA0}', "A"),        // Ạ
    ('\u{1EA1}', "a"),        // ạ
    ('\u{1EA2}', "A"),        // Ả
    ('\u{1EA3}', "a"),        // ả
    ('\u{1EA4}', "A"),        // Ấ
    ('\u{1EA5}', "a"),        // ấ
    ('\u{1EA6}', "A"),        // Ầ
    ('\u{1EA7}', "a"),        // ầ
    ('\u{1EA8}', "A"),        // Ẩ
    ('\u{1EA9}', "a"),        // ẩ
    ('\u{1EAA}', "A"),        // Ẫ
    ('\u{1EAB}', "a"),        // ẫ
    ('\u{1EAC}', "A"),        // Ậ
    ('\u{1EAD}', "a"),        // ậ
    ('\u{1EAE}', "A"),        // Ắ
    ('\u{1EAF}', "a"),        // ắ
    ('\u{1EB0}', "A"),        // Ằ
    ('\u{1EB1}', "a"),        // ằ
    ('\u{1EB2}', "A"),        // Ẳ
    ('\u{1EB3}', "a"),        // ẳ
    ('\u{1EB4}', "A"),        // Ẵ
    ('\u{1EB5}', "a"),        // ẵ
    ('\u{1EB6}', "A"),        // Ặ
    ('\u{1EB7}', "a"),        // ặ
    ('\u{1EB8}', "E"),        // Ẹ
    ('\u{1EB9}', "e"),        // ẹ
    ('\u{1EBA}', "E"),        // Ẻ
    ('\u{1EBB}', "e"),        // ẻ
    ('\u{1EBC}', "E"),        // Ẽ
    ('\u{1EBD}', "e"),        // ẽ
    ('\u{1EBE}', "E"),        // Ế
    ('\u{1EBF}', "e"),        // ế
    ('\u{1EC0}', "E"),        // Ề
    ('\u{1EC1}', "e"),        // ề
    ('\u{1EC2}', "E"),        // Ể
    ('\u{1EC3}', "e"),        // ể
    ('\u{1EC4}', "E"),        // Ễ
    ('\u{1EC5}', "e"),        // ễ
    ('\u{1EC6}', "E"),        // Ệ
    ('\u{1EC7}', "e"),        // ệ
    ('\u{1EC8}', "I"),        // Ỉ
    ('\u{1EC9}', "i"),        // ỉ
    ('\u{1ECA}', "I"),        // Ị
    ('\u{1ECB}', "i"),        // ị
    ('\u{1ECC}', "O"),        // Ọ
    ('\u{1ECD}', "o"),        // ọ
    ('\u{1ECE}', "O"),        // Ỏ
    ('\u{1ECF}', "o"),        // ỏ
    ('\u{1ED0}', "O"),        // Ố
    ('\u{1ED1}', "o"),        // ố
    ('\u{1ED2}', "O"),        // Ồ
    ('\u{1ED3}', "o"),        // ồ
    ('\u{1ED4}', "O"),        // Ổ
    ('\u{1ED5}', "o"),        // ổ
    ('\u{1ED6}', "O"),        // Ỗ
    ('\u{1ED7}', "o"),        // ỗ
    ('\u{1ED8}', "O"),        // Ộ
    ('\u{1ED9}', "o"),        // ộ
    ('\u{1EDA}', "O"),        // Ớ
    ('\u{1EDB}', "o"),        // ớ
    ('\u{1EDC}', "O"),        // Ờ
    ('\u{1EDD}', "o"),        // ờ
    ('\u{1EDE}', "O"),        // Ở
    ('\u{1EDF}', "o"),        // ở
    ('\u{1EE0}', "O"),        // Ỡ
    ('\u{1EE1}', "o"),        // ỡ
    ('\u{1EE2}', "O"),        // Ợ
    ('\u{1EE3}', "o"),        // ợ
    ('\u{1EE4}', "U"),        // Ụ
    ('\u{1EE5}', "u"),        // ụ
    ('\u{1EE6}', "U"),        // Ủ
    ('\u{1EE7}', "u"),        // ủ
    ('\u{1EE8}', "U"),        // Ứ
    ('\u{1EE9}', "u"),        // ứ
    ('\u{1EEA}', "U"),        // Ừ
    ('\u{1EEB}', "u"),        // ừ
    ('\u{1EEC}', "U"),        // Ử
    ('\u{1EED}', "u"),        // ử
    ('\u{1EEE}', "U"),        // Ữ
    ('\u{1EEF}', "u"),        // ữ
    ('\u{1EF0}', "U"),        // Ự
    ('\u{1EF1}', "u"),        // ự
    ('\u{1EF2}', "Y"),        // Ỳ
    ('\u{1EF3}', "y"),        // ỳ
    ('\u{1EF4}', "Y"),        // Ỵ
    ('\u{1EF5}', "y"),        // ỵ
    ('\u{1EF6}', "Y"),        // Ỷ
    ('\u{1EF7}', "y"),        // ỷ
    ('\u{1EF8}', "Y"),        // Ỹ
    ('\u{1EF9}', "y"),        // ỹ
    ('\u{2E18}', ""),         // ⸘
];

/// Checks whether the specified string has diacritics in it.
#[must_use]
pub fn has_diacritics(text: &str) -> bool {
    remove_diacritics(text) != *text || contains_non_conforming_unicode(text)
}

/// The diacritic fold builds before gl-1908 used: NFD-decompose, drop the
/// nonspacing marks of [`is_nonspacing_mark`], fold [`ligature_map`], and
/// recompose (NFC).
///
/// Frozen: the SQLite functions released migration `0031` calls and the
/// one-shot repairs that recognise the keys it gave use it. Never use it for a
/// new write or a query parameter; that is [`remove_diacritics`].
#[must_use]
pub fn remove_diacritics_nfd(text: &str) -> String {
    if text.is_ascii() {
        return text.to_owned();
    }
    let cleaned = strip_non_conforming_unicode(text);
    let map = ligature_map();
    let mut decomposed = String::with_capacity(cleaned.len());
    for ch in cleaned.nfd() {
        if is_nonspacing_mark(ch) {
            continue;
        }
        if let Some(replacement) = map.get(&ch) {
            decomposed.push_str(replacement);
        } else {
            decomposed.push(ch);
        }
    }
    decomposed.nfc().collect()
}

/// The nonspacing marks [`remove_diacritics_nfd`] drops.
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

/// Port of `StringExtensions.Transliterated()`: `text` through ICU's
/// [`DEFAULT_TRANSLITERATOR_ID`] rule, run on ICU4X's CLDR transforms. Every
/// script ICU's `Any-Latin` romanises is romanised (`東京` → `dong jing`),
/// then the text is folded to ASCII, lower-cased, and stripped of nonspacing
/// marks and punctuation. Thai, which ICU4X has no transform for, keeps its
/// letters.
#[must_use]
pub fn transliterated(text: &str) -> String {
    crate::transliteration::transliterate(text)
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
/// a space here exactly as .NET does it, and so is a combining mark the
/// diacritic fold keeps (`Saldan\u{303}a` stores as `saldan a`). A character
/// outside the Basic Multilingual Plane is a space too, letter or not: .NET's
/// regex sees its two surrogates (`𠀀大` stores as `大`).
///
/// Returns the original string unchanged if it is empty or all whitespace.
#[must_use]
pub fn get_clean_value(value: &str) -> String {
    clean_value_with(value, remove_diacritics, &NOT_LETTER_DIGIT_SPACE)
}

/// [`get_clean_value`] as builds before gl-1908 derived it: over
/// [`remove_diacritics_nfd`], with a character outside the Basic Multilingual
/// Plane kept when it is a letter or a number. Kept only so the one-shot repair
/// can recognise the clean values they stored. Never use it for a new write or
/// a query parameter.
#[must_use]
pub fn get_clean_value_nfd(value: &str) -> String {
    clean_value_with(
        value,
        remove_diacritics_nfd,
        &NOT_LETTER_DIGIT_SPACE_BY_CODE_POINT,
    )
}

fn clean_value_with(value: &str, fold: fn(&str) -> String, not_word: &Regex) -> String {
    if value.trim().is_empty() {
        return value.to_owned();
    }
    let cleaned = lower_invariant(&fold(value));
    let cleaned = not_word.replace_all(&cleaned, " ");
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
/// uppercase mapping as [`equals_ordinal_ignore_case`], then the characters
/// compared by scalar value. Unlike `string.CompareOrdinal`, which compares
/// UTF-16 code units, .NET's ignore-case comparison sorts a supplementary-plane
/// character after U+E000-U+FFFF (measured on .NET 10), so the order is the
/// byte order of [`ordinal_ignore_case_key`].
#[must_use]
pub fn compare_ordinal_ignore_case(left: &str, right: &str) -> std::cmp::Ordering {
    left.chars()
        .map(to_upper_ordinal)
        .cmp(right.chars().map(to_upper_ordinal))
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

/// `string.Equals(a, b, StringComparison.InvariantCultureIgnoreCase)` as .NET
/// computes it with ICU: root collation at the secondary strength that
/// `CompareOptions.IgnoreCase` selects. Case alone never differs (`ärne`,
/// `Ärne`), and neither do canonically equivalent spellings, ignorable
/// characters or compatibility variants (`ﬁ`, `fi`); accents, `ß` against `ss`
/// and the dotless `ı` against `I` do. Unlike .NET, character width and kana
/// type do not count (`ｂ` against `b`, `あ` against `ア`): see gl-1793.
///
/// The collator reads ICU4X's baked root data; a build without it compares
/// as [`equals_ordinal_ignore_case`] rather than failing.
#[must_use]
pub fn equals_invariant_ignore_case(left: &str, right: &str) -> bool {
    // shortcut: .NET adds tailoring rules that keep character width and kana type apart
    // (`ｂ` and `b`, `あ` and `ア` differ there, equal here); ICU4X takes no runtime rules,
    // so porting them needs their tables; replace this when gl-1793 lands.
    static COLLATOR: OnceLock<Option<icu_collator::CollatorBorrowed<'static>>> = OnceLock::new();
    COLLATOR
        .get_or_init(|| {
            let mut options = icu_collator::options::CollatorOptions::default();
            options.strength = Some(icu_collator::options::Strength::Secondary);
            icu_collator::Collator::try_new(icu_collator::CollatorPreferences::default(), options)
                .ok()
        })
        .as_ref()
        .map_or_else(
            || equals_ordinal_ignore_case(left, right),
            |collator| collator.compare(left, right).is_eq(),
        )
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
/// it differs from `ToUpperInvariant`). For ports that compare `char` by `char`;
/// whole strings go through [`equals_ordinal_ignore_case`].
#[must_use]
pub fn to_upper_ordinal(c: char) -> char {
    if c.is_ascii() {
        c.to_ascii_uppercase()
    } else if c == '\u{017f}' {
        c
    } else {
        to_upper_invariant(c)
    }
}

/// .NET's invariant uppercase of one Unicode scalar (`char.ToUpperInvariant`,
/// or `Rune.ToUpperInvariant` beyond the BMP): the simple uppercase, keeping
/// the dotless `ı` (U+0131) and the pairs newer than .NET's ICU. Whole strings
/// go through [`upper_invariant`].
#[must_use]
pub fn to_upper_invariant(c: char) -> char {
    // .NET ChangeCaseInvariant preserves dotless i, unlike Unicode's default.
    if c.is_ascii() {
        c.to_ascii_uppercase()
    } else if c == '\u{0131}' || NEWER_UNICODE_CASE_PAIRS.binary_search(&c).is_ok() {
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

/// .NET's zero-placeholder custom format of an integer in the invariant
/// culture (`n.ToString("000")`, `"0000"`): the digits zero-padded to
/// `digits`, then the sign before them. Rust's `{:03}` counts the sign in the
/// width, so `-1` is `-01` there and `-001` here.
#[must_use]
pub fn zero_padded(value: i64, digits: usize) -> String {
    let sign = if value < 0 { "-" } else { "" };
    format!("{sign}{:0digits$}", value.unsigned_abs())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rstest::rstest;

    // The C# output is the oracle: `(-1).ToString("000")` is `-001`.
    #[rstest]
    #[case(-1, 3, "-001")]
    #[case(0, 3, "000")]
    #[case(7, 3, "007")]
    #[case(1000, 3, "1000")]
    #[case(-1, 4, "-0001")]
    #[case(-12345, 4, "-12345")]
    #[case(i64::MIN, 3, "-9223372036854775808")]
    fn zero_padded_matches_dotnet_custom_zero_format(
        #[case] value: i64,
        #[case] digits: usize,
        #[case] expected: &str,
    ) {
        assert_eq!(zero_padded(value, digits), expected);
    }

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
            // The per-character folds too: the strings above take an ASCII
            // shortcut that never reaches them.
            let upper = char::from_u32(upper).unwrap();
            assert_eq!(
                to_upper_invariant(c),
                upper,
                "char uppercase U+{scalar:04X}"
            );
            let ordinal = if c == '\u{017f}' { c } else { upper };
            assert_eq!(to_upper_ordinal(c), ordinal, "ordinal U+{scalar:04X}");
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

    /// The ASCII fast path returns what the table would: no key is ASCII, so
    /// every ASCII char, all of them together, and the ASCII on both sides of a
    /// mapped char come through unchanged. The same holds for the frozen fold.
    #[test]
    fn remove_diacritics_ascii_fast_path_matches_the_table() {
        let all_ascii: String = (0u8..=127).map(char::from).collect();
        assert!(all_ascii.chars().all(|c| diacritic_base(c).is_none()));
        assert_eq!(remove_diacritics(&all_ascii), all_ascii);
        assert_eq!(remove_diacritics_nfd(&all_ascii), all_ascii);
        for (mixed, folded) in [
            ("é", "e"),
            ("e\u{301}", "e\u{301}"),
            ("\u{FFFD}", ""),
            ("운", "운"),
        ] {
            let input = format!("{all_ascii}{mixed}{all_ascii}");
            assert_eq!(
                remove_diacritics(&input),
                format!("{all_ascii}{folded}{all_ascii}"),
                "{mixed:?}"
            );
        }
    }

    /// The table changes exactly the 282 characters the package's
    /// `RemoveDiacritics` changes when run alone over every BMP code unit
    /// (gl-1908), and each of them to one short base.
    #[test]
    fn the_table_maps_what_the_diacritics_package_maps() {
        let mapped: Vec<char> = (0..=0xFFFF_u32)
            .filter_map(char::from_u32)
            .filter(|&c| diacritic_base(c).is_some())
            .collect();
        assert_eq!(mapped.len(), 282);
        assert!(
            DIACRITICS.windows(2).all(|pair| pair[0].0 < pair[1].0),
            "the binary search needs the keys in order"
        );
        assert!(
            mapped
                .iter()
                .all(|&c| remove_diacritics(&c.to_string()) != c.to_string())
        );
        assert_eq!(diacritic_base('ß'), None, "the package maps ß to itself");
    }

    /// Jellyfin 12.4's own `RemoveDiacritics` and `GetCleanValue`, run from
    /// the assemblies its image ships (gl-1908): combining marks are kept
    /// (only the combining cedilla goes), letters outside the package's table
    /// are kept, and a kept mark becomes a space in the clean value.
    #[rstest]
    #[case("Zoë Saldaña", "Zoe Saldana", "zoe saldana")]
    #[case(
        "Zoe\u{308} Saldan\u{303}a",
        "Zoe\u{308} Saldan\u{303}a",
        "zoe saldan a"
    )]
    #[case("Zoë Saldan\u{303}a", "Zoe Saldan\u{303}a", "zoe saldan a")]
    #[case(
        "\u{5E9}\u{5B8}\u{5C1}\u{5DC}\u{5D5}\u{5B9}\u{5DD}",
        "\u{5E9}\u{5B8}\u{5C1}\u{5DC}\u{5D5}\u{5B9}\u{5DD}",
        "ש לו ם"
    )]
    #[case(
        "\u{645}\u{64F}\u{62D}\u{64E}\u{645}\u{651}\u{64E}\u{62F}",
        "\u{645}\u{64F}\u{62D}\u{64E}\u{645}\u{651}\u{64E}\u{62F}",
        "م ح م د"
    )]
    #[case("בראשית", "בראשית", "בראשית")]
    #[case("Straße", "Straße", "straße")]
    #[case("STRA\u{1E9E}E", "STRAßE", "straße")]
    #[case("¡Three Amigos!", "Three Amigos!", "three amigos")]
    #[case("¿Quién?", "Quien?", "quien")]
    #[case("Esperanto Ĉeĥo", "Esperanto Ĉeĥo", "esperanto ĉeĥo")]
    #[case("Wǒ Ài Nǐ", "Wǒ Ai Nǐ", "wǒ ai nǐ")]
    #[case("Țară", "Tara", "tara")]
    #[case("Ţară", "Ţara", "ţara")]
    #[case("Muškatų", "Muskatų", "muskatų")]
    #[case("Fran\u{63}\u{327}ois", "Francois", "francois")]
    #[case("Αθήνα", "Αθηνα", "αθηνα")]
    #[case("Ἀθῆναι", "Ἀθῆναι", "ἀθῆναι")]
    #[case("Андрей", "Андреи", "андреи")]
    #[case("Ёлка", "Елка", "елка")]
    #[case("ѓуб", "ѓуб", "ѓуб")]
    #[case("Mañana ª º", "Manana a o", "manana a o")]
    #[case("İstanbul ı", "Istanbul i", "istanbul i")]
    #[case("Əli ə", "Ali a", "ali a")]
    #[case("Đorđe", "Dorde", "dorde")]
    #[case("Æon Fløde Œuvre Łódź", "Æon Flode Œuvre Lodz", "æon flode œuvre lodz")]
    #[case("Ðð Þþ", "Ðð Þþ", "ðð þþ")]
    #[case("الموسيقى", "الموسيقي", "الموسيقي")]
    #[case("ٱلله", "الله", "الله")]
    #[case("ஔவை", "ஒவை", "ஒவ")]
    #[case("\u{212B}\u{2126}\u{212A}", "\u{212B}\u{2126}\u{212A}", "åωk")]
    #[case("\u{F900}", "\u{F900}", "\u{F900}")]
    #[case("Tiếng Việt", "Tieng Viet", "tieng viet")]
    #[case("\u{FB2A}לום", "\u{FB2A}לום", "\u{FB2A}לום")]
    #[case("किताब", "किताब", "क त ब")]
    #[case("é\u{301}", "e\u{301}", "e")]
    // Outside the Basic Multilingual Plane every character is two surrogates
    // to .NET's regex, so letters and numbers there clean to spaces too.
    #[case("\u{10400} Zoë", "\u{10400} Zoe", "zoe")]
    #[case("\u{20000}大", "\u{20000}大", "大")]
    #[case("\u{1D400}bc", "\u{1D400}bc", "bc")]
    #[case("a\u{1F600}b", "a\u{1F600}b", "a b")]
    #[case("x\u{1D7CE}2", "x\u{1D7CE}2", "x 2")]
    fn diacritics_match_jellyfin_12_4(
        #[case] input: &str,
        #[case] removed: &str,
        #[case] clean: &str,
    ) {
        assert_eq!(remove_diacritics(input), removed, "{input}");
        assert_eq!(get_clean_value(input), clean, "{input}");
    }

    /// The frozen fold keeps giving what builds before gl-1908 stored.
    #[rstest]
    #[case("Zoe\u{308} Saldan\u{303}a", "Zoe Saldana", "zoe saldana")]
    #[case("Straße", "Strasse", "strasse")]
    #[case("Æon Œuvre", "AEon OEuvre", "aeon oeuvre")]
    #[case("\u{5E9}\u{5B8}\u{5C1}\u{5DC}\u{5D5}\u{5B9}\u{5DD}", "שלום", "שלום")]
    #[case("Esperanto Ĉeĥo", "Esperanto Ceho", "esperanto ceho")]
    #[case("Đorđe ı", "Đorđe ı", "đorđe ı")]
    #[case("\u{212B}", "A", "a")]
    #[case("Gísli Örn Garðarsson", "Gisli Orn Garðarsson", "gisli orn garðarsson")]
    #[case("a\u{FFFD}b", "ab", "ab")]
    #[case("\u{10400} Zoë", "\u{10400} Zoe", "\u{10428} zoe")]
    fn the_nfd_fold_is_frozen(#[case] input: &str, #[case] removed: &str, #[case] clean: &str) {
        assert_eq!(remove_diacritics_nfd(input), removed, "{input}");
        assert_eq!(get_clean_value_nfd(input), clean, "{input}");
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
    #[case("Béla Tarr", "bela tarr")] // folded and lower-cased
    #[case("Indiana Jones: Raiders", "indiana jones raiders")] // punctuation removed
    #[case("東京", "dong jing")] // romanised
    fn transliterated_runs_the_icu_rule(#[case] input: &str, #[case] expected: &str) {
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
    #[case("𐐀", "")] // .NET's regex sees two surrogates, neither a letter
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

    // Expected values from .NET 10.0.13's native ICU collation
    // (`GlobalizationNative_CompareString` on the invariant sort handle with
    // `CompareOptions.IgnoreCase`, ICU 78.3), which `string.Equals` with
    // `InvariantCultureIgnoreCase` returns.
    #[rstest]
    #[case("ärne", "Ärne", true)]
    #[case("admin", "Admin", true)]
    #[case("ΣΊΣΥΦΟΣ", "σίσυφος", true)] // the final sigma too
    #[case("\u{1c4}", "\u{1c6}", true)]
    #[case("\u{10428}", "\u{10400}", true)] // a supplementary-plane letter (Deseret)
    #[case("\u{212a}", "k", true)] // the Kelvin sign, unlike OrdinalIgnoreCase
    #[case("ﬁ", "fi", true)]
    #[case("²", "2", true)]
    #[case("a\u{301}", "á", true)] // canonically equivalent
    #[case("Bob\u{200b}", "Bob", true)] // an ignorable character
    #[case("Ab\u{ad}", "ab", true)]
    #[case("Bob", "Bób", false)]
    #[case("Ärne", "Arne", false)]
    #[case("bob", "bob ", false)]
    #[case("ß", "ss", false)]
    #[case("straße", "STRASSE", false)]
    #[case("\u{17f}", "s", false)]
    #[case("\u{131}", "I", false)]
    #[case("i", "\u{130}", false)]
    #[case("æ", "AE", false)]
    #[case("Œ", "oe", false)]
    fn invariant_ignore_case_matches_dotnet(
        #[case] left: &str,
        #[case] right: &str,
        #[case] equal: bool,
    ) {
        assert_eq!(equals_invariant_ignore_case(left, right), equal);
        assert_eq!(equals_invariant_ignore_case(right, left), equal);
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
        // Scalar order, not UTF-16 order: .NET 10 sorts U+1F3AC after U+FF21 `Ａ`
        // and U+10000 after U+E000, although their surrogates (0xD83C, 0xD800)
        // are lower code units.
        assert_eq!(
            compare_ordinal_ignore_case("\u{1F3AC}", "\u{FF21}"),
            Greater
        );
        assert_eq!(
            compare_ordinal_ignore_case("\u{10000}", "\u{E000}"),
            Greater
        );
        assert_eq!(compare_ordinal_ignore_case("\u{E000}", "\u{10000}"), Less);
        assert_eq!(compare_ordinal_ignore_case("\u{10428}", "\u{10400}"), Equal);
        assert_eq!(compare_ordinal_ignore_case("ab", "a"), Greater);
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
