//! Upstream's sort-name transliterator, the ICU rule
//! `Any-Latin; Latin-Ascii; Lower; NFD; [:Nonspacing Mark:] Remove; [:Punctuation:] Remove;`
//! (`StringExtensions.Transliterated`, ICU4N 60.1), on ICU4X's CLDR transforms.
//!
//! ICU4X has no `Any-Latin`: ICU implements it in code (`AnyTransliterator`),
//! not as a rule file. [`any_latin`] ports it. The text is cut into script
//! runs, each taking in the Common and Inherited characters on either side,
//! and each run goes through its script's `<Script>-Latin` transform from
//! left to right, seeing the converted character before it: Han-Latin puts a
//! space before a syllable that follows a letter, so `ジョジョの奇妙` becomes
//! `jojono qi miao`.
//!
//! Only the scripts ICU's `Any-Latin` finds a transform for are romanised
//! ([`ROMANISED`]). Known difference from Jellyfin: ICU4X ships no
//! `Thai-Latin`, so Thai keeps its letters (Jellyfin turns `ภาพยนตร์` into
//! `phaphyntr`); gl-1907 tracks it.

use std::cell::RefCell;
use std::collections::HashMap;

use icu_casemap::CaseMapper;
use icu_experimental::transliterate::Transliterator;
use icu_locale_core::{LanguageIdentifier, Locale};
use icu_properties::props::{GeneralCategory, GeneralCategoryGroup, Script};
use icu_properties::{CodePointMapData, PropertyNamesShort};
use unicode_normalization::UnicodeNormalization;

/// The `Latin-ASCII` transform's id.
const LATIN_ASCII: &str = "und-t-und-latn-d0-ascii";

/// The scripts ICU 60's `Any-Latin` romanises: those it finds a
/// `<Script>-Latin` transform for by the script's name, measured on Jellyfin
/// 12.4. It finds none for Syriac and Canadian Aboriginal, which ICU4X has
/// transforms for, nor for Ethiopic, Myanmar, Khmer, Lao, Tibetan, Sinhala,
/// Mongolian and the rest, so those stay as written, as there. Thai is
/// romanised there but has no ICU4X transform.
const ROMANISED: &[Script] = &[
    Script::Arabic,
    Script::Armenian,
    Script::Bengali,
    Script::Bopomofo,
    Script::Cyrillic,
    Script::Devanagari,
    Script::Georgian,
    Script::Greek,
    Script::Gujarati,
    Script::Gurmukhi,
    Script::Han,
    Script::Hangul,
    Script::Hebrew,
    Script::Hiragana,
    Script::Kannada,
    Script::Katakana,
    Script::Malayalam,
    Script::Oriya,
    Script::Tamil,
    Script::Telugu,
    Script::Thaana,
    Script::Thai,
];

thread_local! {
    // A `Transliterator` is neither `Send` nor `Sync`, so each thread builds
    // its own, once, on first use. `None`: ICU4X has no transform for it.
    static TRANSFORMS: RefCell<HashMap<Option<Script>, Option<Transliterator>>> =
        RefCell::default();
}

/// Upstream's `Transliterated()`: ICU's
/// `Any-Latin; Latin-Ascii; Lower; NFD; [:Nonspacing Mark:] Remove; [:Punctuation:] Remove;`.
pub(crate) fn transliterate(text: &str) -> String {
    let ascii = apply(None, any_latin(text));
    let lower = CaseMapper::new().lowercase_to_string(&ascii, &LanguageIdentifier::UNKNOWN);
    let categories = CodePointMapData::<GeneralCategory>::new();
    lower
        .nfd()
        .filter(|&c| {
            let category = categories.get(c);
            category != GeneralCategory::NonspacingMark
                && !GeneralCategoryGroup::Punctuation.contains(category)
        })
        .collect()
}

/// ICU's `Any-Latin` (`AnyTransliterator` with its `ScriptRunIterator`).
fn any_latin(text: &str) -> String {
    let scripts = CodePointMapData::<Script>::new();
    let joins = |c: char| matches!(scripts.get(c), Script::Common | Script::Inherited);
    let mut converted = String::with_capacity(text.len());
    let fixed = width_fix(text);
    let mut rest = fixed.as_str();
    while !rest.is_empty() {
        // A run starts back over the Common and Inherited characters the
        // previous one ended with, as converted.
        let back: usize = converted
            .chars()
            .rev()
            .take_while(|&c| joins(c))
            .map(char::len_utf8)
            .sum();
        let mut run = converted.split_off(converted.len() - back);
        let mut script = None;
        let end = rest
            .char_indices()
            .find(|&(_, c)| {
                let of = scripts.get(c);
                !joins(c) && *script.get_or_insert(of) != of
            })
            .map_or(rest.len(), |(at, _)| at);
        run.push_str(&rest[..end]);
        rest = &rest[end..];
        converted = to_latin(script, converted, &run);
    }
    converted
}

/// The width fix ICU's `Any-Latin` puts before every run's transform when the
/// target is not a wide script: `[[:dt=Nar:][:dt=Wide:]] nfkd`. The
/// characters with a `<wide>` or `<narrow>` decomposition are U+3000 and the
/// Halfwidth and Fullwidth Forms block's (ICU4X has no `Decomposition_Type`).
fn width_fix(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c == '\u{3000}' || ('\u{FF00}'..='\u{FFEF}').contains(&c) {
            out.extend(c.nfkd());
        } else {
            out.push(c);
        }
    }
    out
}

/// `run` through `script`'s `<Script>-Latin` transform, appended to
/// `converted`, the text before it. The transform sees the last character
/// before the run as context: ICU's rules look that far back across a run's
/// start (Han-Latin's space after a letter), and a transform rewrites only its
/// own script, so that character comes back as it was. More context would only
/// cost time, as ICU4X's transforms walk every character they are given. A
/// run of Common characters only, or of a script not [`ROMANISED`] (Latin
/// among them), is appended as it is.
fn to_latin(script: Option<Script>, mut converted: String, run: &str) -> String {
    if script.is_some_and(|script| ROMANISED.contains(&script)) {
        let last = converted
            .char_indices()
            .next_back()
            .map_or(converted.len(), |(at, _)| at);
        let context = converted.split_off(last);
        converted.push_str(&apply(script, context + run));
    } else {
        converted.push_str(run);
    }
    converted
}

/// `text` through the `<Script>-Latin` transform of `script`, or `Latin-ASCII`
/// for `None`; unchanged where ICU4X has no such transform.
fn apply(script: Option<Script>, text: String) -> String {
    TRANSFORMS.with_borrow_mut(|transforms| {
        match transforms.entry(script).or_insert_with(|| load(script)) {
            Some(transform) => transform.transliterate(text),
            None => text,
        }
    })
}

/// Builds the transform [`apply`] runs for `script`.
fn load(script: Option<Script>) -> Option<Transliterator> {
    let id = match script {
        None => LATIN_ASCII.to_owned(),
        // CLDR keys its Han-Latin (ICU's, pinyin for simplified and
        // traditional characters alike) by Simplified Han.
        Some(Script::Han) => "und-Latn-t-und-hans".to_owned(),
        Some(script) => {
            let code = PropertyNamesShort::<Script>::new().get_locale_script(script)?;
            format!("und-Latn-t-und-{}", code.as_str().to_ascii_lowercase())
        }
    };
    Transliterator::try_new(&Locale::try_from_str(&id).ok()?).ok()
}

#[cfg(test)]
mod tests {
    use super::{ROMANISED, load, transliterate};
    use icu_properties::props::Script;
    use rstest::rstest;

    /// Every romanised script but Thai, and `Latin-ASCII`, has its transform
    /// in ICU4X's data; a missing one would leave its script as written.
    #[test]
    fn the_transforms_load() {
        assert!(load(None).is_some());
        for script in ROMANISED
            .iter()
            .copied()
            .filter(|&script| script != Script::Thai)
        {
            assert!(load(Some(script)).is_some(), "{script:?}");
        }
    }

    /// The known gap: ICU4X has no Thai-Latin, so Thai keeps its letters and
    /// loses only its nonspacing marks (Jellyfin 12.4 gives `phaphyntr`).
    #[test]
    fn thai_keeps_its_letters() {
        assert!(load(Some(Script::Thai)).is_none());
        assert_eq!(transliterate("ภาพยนตร์"), "ภาพยนตร");
    }

    /// The tail of the rule on Latin text: `Latin-ASCII`, lower case, and
    /// marks and punctuation removed.
    #[rstest]
    #[case("Béla Tarr", "bela tarr")]
    #[case("Garðarsson: Þór!", "gardarsson thor")]
    #[case("«Брат»", "<<brat>>")]
    fn the_rule_tail_folds_cases_marks_and_punctuation(
        #[case] input: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(transliterate(input), expected);
    }
}
