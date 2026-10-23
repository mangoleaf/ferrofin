//! Sort-name derivation — port of C# `BaseItem.GetSortName` / `CreateSortName` /
//! `ModifySortChunks` (Jellyfin 12.0 `MediaBrowser.Controller/Entities/BaseItem.cs`),
//! with the server's `SortRemoveWords` / `SortRemoveCharacters` /
//! `SortReplaceCharacters` ([`SortRules`], installed by the server with
//! [`install_sort_rules`]; Jellyfin's defaults until then).

use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use crate::string_extensions::{lower_invariant, remove_diacritics};

/// `ServerConfiguration.SortRemoveWords` defaults — leading/interior/trailing
/// articles.
const SORT_REMOVE_WORDS: [&str; 3] = ["the", "a", "an"];

/// `ServerConfiguration.SortRemoveCharacters` defaults — deleted outright.
const SORT_REMOVE_CHARACTERS: [&str; 6] = [",", "&", "-", "{", "}", "'"];

/// `ServerConfiguration.SortReplaceCharacters` defaults — each becomes a space.
const SORT_REPLACE_CHARACTERS: [&str; 3] = [".", "+", "%"];

/// The three lists `GetSortName` reads from `ServerConfiguration`: the words
/// removed where they stand alone, the strings deleted, and the strings
/// turned into a space. Each entry is matched as the exact string, as C#'s
/// ordinal `StartsWith`/`Replace` do, against the lower-cased name, so an
/// entry with an upper-case letter never matches, as upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SortRules {
    /// `SortRemoveWords`.
    pub remove_words: Vec<String>,
    /// `SortRemoveCharacters`.
    pub remove_characters: Vec<String>,
    /// `SortReplaceCharacters`.
    pub replace_characters: Vec<String>,
}

impl Default for SortRules {
    /// Jellyfin's defaults (`ServerConfiguration`'s initializers).
    fn default() -> Self {
        let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
        Self {
            remove_words: owned(&SORT_REMOVE_WORDS),
            remove_characters: owned(&SORT_REMOVE_CHARACTERS),
            replace_characters: owned(&SORT_REPLACE_CHARACTERS),
        }
    }
}

/// The rules every derivation reads ([`get_sort_name`] and the helpers on
/// it), as `GetSortName` reads `ConfigurationManager.Configuration` on each
/// call. Jellyfin's defaults until the server installs its own.
static INSTALLED: LazyLock<RwLock<Arc<SortRules>>> =
    LazyLock::new(|| RwLock::new(Arc::new(SortRules::default())));

/// Makes `rules` the ones every later derivation uses: the server calls it
/// with its configuration at startup and again on each save. Stored sort
/// names are left as they are, as upstream leaves them until an item is
/// refreshed.
pub fn install_sort_rules(rules: SortRules) {
    *INSTALLED.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(rules);
}

/// The rules installed now.
fn installed_rules() -> Arc<SortRules> {
    Arc::clone(&INSTALLED.read().unwrap_or_else(PoisonError::into_inner))
}

/// Port of C# `BaseItem.GetSortName(name, enableAlphaNumericSorting, config)`
/// (Jellyfin 12.0 `MediaBrowser.Controller/Entities/BaseItem.cs:992-1035`) —
/// the ONE cleaning rule behind both an auto-generated `SortName` and a
/// user-supplied `ForcedSortName`.
///
/// With `enable_alpha_numeric_sorting` off (only `Person` overrides it to
/// `false`) the answer is `name.TrimStart()` — the name verbatim. Otherwise:
/// trim and lower-case (.NET invariant simple casing), remove each article where it stands as a whole word
/// (at the start, surrounded by spaces, or at the end), delete the
/// remove-character set, turn each replace-character into a space, then
/// [`modify_sort_chunks`] left-pads every run of digits to 10 so numbers sort
/// naturally (`Movie 0001 (2020)` → `movie 0000000001 (0000002020)`).
///
/// **The stage order is load-bearing and matches C#: words, then removes, then
/// replaces.** Doing characters first changes the answer for real titles —
/// `A.I. Artificial Intelligence` replaces `.`→space into `a i  artificial …`,
/// which then *starts* with the article `a` and gets it stripped. C# strips
/// articles while the `.` is still attached, so nothing matches.
///
/// The lists are the installed [`SortRules`] (see [`get_sort_name_with_rules`]).
#[must_use]
pub fn get_sort_name(name: &str, enable_alpha_numeric_sorting: bool) -> String {
    get_sort_name_with_rules(name, enable_alpha_numeric_sorting, &installed_rules())
}

/// [`get_sort_name`] over the given `rules` rather than the installed ones.
///
/// An empty entry in the remove or replace lists is skipped: C#'s
/// `string.Replace` throws on an empty search string, which fails the item's
/// save upstream, and Rust's would insert the replacement between every
/// character. An empty remove *word* is applied, as upstream applies it (it
/// turns each double space into one).
#[must_use]
pub fn get_sort_name_with_rules(
    name: &str,
    enable_alpha_numeric_sorting: bool,
    rules: &SortRules,
) -> String {
    get_sort_name_with(name, enable_alpha_numeric_sorting, lower_invariant, rules)
}

/// The derivation before invariant casing (full Unicode `to_lowercase`), kept
/// only so migration `0031` can recognise keys Ferrofin wrote with it, always
/// over Jellyfin's default rules (Ferrofin wrote those keys before it read the
/// configured ones). Never use it for a new write or a query parameter.
#[must_use]
pub fn previous_create_sort_name(name: &str) -> String {
    get_sort_name_with(name, true, str::to_lowercase, &SortRules::default())
}

/// [`create_sort_name`] over Jellyfin's default rules, whatever is installed:
/// the `ferrofin_sort_name` SQLite function released migrations call, which
/// must keep giving the answer they were written against (and stay
/// deterministic, as the function is registered).
#[must_use]
pub fn default_rules_sort_name(name: &str) -> String {
    get_sort_name_with_rules(name, true, &SortRules::default())
}

/// [`forced_sort_key`] over Jellyfin's default rules: the
/// `ferrofin_forced_sort_key` SQLite function, for the same reason as
/// [`default_rules_sort_name`].
#[must_use]
pub fn default_rules_forced_sort_key(forced: &str) -> String {
    get_sort_name_with_rules(forced, true, &SortRules::default())
}

/// The 10.11.8-era forced key — `ModifySortChunks(ForcedSortName)` through
/// full Unicode `to_lowercase` — kept only so migration `0031` can recognise
/// stored values written with it.
#[must_use]
pub fn previous_forced_sort_key(forced: &str) -> String {
    modify_sort_chunks(forced).to_lowercase()
}

fn get_sort_name_with(
    name: &str,
    enable_alpha_numeric_sorting: bool,
    lowercase: fn(&str) -> String,
    rules: &SortRules,
) -> String {
    if !enable_alpha_numeric_sorting {
        return name.trim_start().to_owned();
    }
    let mut sortable = lowercase(name.trim());
    for search in &rules.remove_words {
        if let Some(rest) = sortable.strip_prefix(&format!("{search} ")) {
            sortable = rest.to_owned();
        }
        sortable = sortable.replace(&format!(" {search} "), " ");
        if let Some(rest) = sortable.strip_suffix(&format!(" {search}")) {
            sortable = rest.to_owned();
        }
    }
    for remove in rules.remove_characters.iter().filter(|s| !s.is_empty()) {
        sortable = sortable.replace(remove.as_str(), "");
    }
    for replace in rules.replace_characters.iter().filter(|s| !s.is_empty()) {
        sortable = sortable.replace(replace.as_str(), " ");
    }
    modify_sort_chunks(&sortable)
}

/// Port of C# `BaseItem.CreateSortName` for a kind with alphanumeric sorting
/// on — [`get_sort_name`] with `enable_alpha_numeric_sorting = true`.
///
/// Callers that know the item's kind should go through the kind-aware wrapper
/// in `ferrofin-core` (`kinds::sort_name_for`), which routes `Person` down the
/// verbatim branch.
#[must_use]
pub fn create_sort_name(name: &str) -> String {
    get_sort_name(name, true)
}

/// The sort key C# derives from a non-empty `ForcedSortName` for a kind with
/// alphanumeric sorting on.
///
/// Since Jellyfin 12.0 (`BaseItem.cs:549`, jellyfin#17388) a forced sort name
/// runs through the SAME pipeline as an auto-generated one — `GetSortName(
/// ForcedSortName, EnableAlphaNumericSorting, config)` — so `"The Spider-Man:
/// Homecoming"` sorts as `spiderman: homecoming` whether it came from the
/// title or the user's override, and the two land together. 10.11.8 only did
/// `ModifySortChunks(ForcedSortName).ToLowerInvariant()`, keeping the article
/// and the punctuation; the one-shot `repair_forced_sort_names` pass in
/// `ferrofin-core` moves stored rows across.
///
/// Callers that know the kind use `kinds::forced_sort_name_for`, which wires
/// the `Person` exception (`TrimStart()` only).
#[must_use]
pub fn forced_sort_key(forced: &str) -> String {
    get_sort_name(forced, true)
}

/// Left-pads each maximal run of ASCII digits in `name` to width 10 with `0`,
/// then strips diacritics.
///
/// Port of `BaseItem.ModifySortChunks`.
///
/// TODO(open work, not an accepted divergence): two steps of the C# are still
/// missing, and both change the sort key a client sees.
///
/// 1. Upstream closes with `if (!result.All(char.IsAscii)) result.Transliterated()`
///    — an ICU romanization of whatever is still non-ASCII after the strip. A
///    Cyrillic, Greek or CJK title therefore sorts differently here than on
///    Jellyfin, and `SortName` drives the client play queue. Porting it means
///    taking an ICU transliteration dependency, which is the owner's call to
///    make; raise it rather than leaving this note to rot.
/// 2. C# `char.IsDigit` matches the whole Unicode `Nd` category, not just
///    ASCII — so an Arabic-Indic or fullwidth digit run goes unpadded here.
///    That one is a local fix (`char::is_numeric` plus a width decision).
fn modify_sort_chunks(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_ascii_digit() {
            let mut digits = String::new();
            while chars.peek().is_some_and(char::is_ascii_digit) {
                digits.push(chars.next().unwrap_or_default());
            }
            for _ in digits.len()..10 {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            out.push(c);
            chars.next();
        }
    }
    remove_diacritics(&out)
}

#[cfg(test)]
mod tests {
    use rstest::rstest;

    use super::{
        SortRules, create_sort_name, forced_sort_key, get_sort_name, get_sort_name_with_rules,
        modify_sort_chunks,
    };

    fn rules(words: &[&str], removes: &[&str], replaces: &[&str]) -> SortRules {
        let owned = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect();
        SortRules {
            remove_words: owned(words),
            remove_characters: owned(removes),
            replace_characters: owned(replaces),
        }
    }

    /// `GetSortName` reads the three lists from the configuration, so an
    /// operator's lists decide the key: German articles go, English ones
    /// stay; emptied lists keep every word and character.
    #[rstest]
    #[case(&["der", "die", "das"], &[], &[], "Der Untergang", "untergang")]
    #[case(&["der", "die", "das"], &[], &[], "The Matrix", "the matrix")]
    #[case(&[], &[], &[], "The Spider-Man: Homecoming", "the spider-man: homecoming")]
    #[case(&[], &["-", ":"], &[], "The Spider-Man: Homecoming", "the spiderman homecoming")]
    #[case(&[], &[], &["-"], "Spider-Man", "spider man")]
    // Entries are strings, matched whole (ordinal `Replace`).
    #[case(&[], &["::"], &[], "a::b:c", "ab:c")]
    // An upper-case entry never matches the lower-cased name, as upstream.
    #[case(&["The"], &[], &[], "The Matrix", "the matrix")]
    // An empty character entry is skipped (C# throws on it).
    #[case(&[], &[""], &[""], "Up", "up")]
    fn the_configured_lists_decide_the_sort_name(
        #[case] words: &[&str],
        #[case] removes: &[&str],
        #[case] replaces: &[&str],
        #[case] name: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(
            get_sort_name_with_rules(name, true, &rules(words, removes, replaces)),
            expected
        );
    }

    /// The defaults are Jellyfin's `ServerConfiguration` initializers.
    #[test]
    fn the_default_rules_are_jellyfins() {
        assert_eq!(
            SortRules::default(),
            rules(
                &["the", "a", "an"],
                &[",", "&", "-", "{", "}", "'"],
                &[".", "+", "%"]
            )
        );
    }

    // The upstream xUnit oracle, transliterated verbatim
    // (`Jellyfin.Controller.Tests/Entities/BaseItemTests.cs`
    // `BaseItem_ModifySortChunks_Valid`).
    #[rstest]
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
    fn create_sort_name_matches_jellyfin() {
        assert_eq!(
            create_sort_name("Movie 0001 (2020)"),
            "movie 0000000001 (0000002020)"
        );
        assert_eq!(create_sort_name("The Matrix"), "matrix");
        assert_eq!(create_sort_name("A Beautiful Mind"), "beautiful mind");
        assert_eq!(create_sort_name("An Education"), "education");
        assert_eq!(create_sort_name("Theatre of Blood"), "theatre of blood");
        assert_eq!(create_sort_name("Se7en"), "se0000000007en");
    }

    /// C# removes an article anywhere it stands as a whole word, not just at
    /// the front — `sortable.Replace(" the ", " ")` and the `EndsWith` arm.
    #[test]
    fn articles_are_removed_in_the_middle_and_at_the_end() {
        assert_eq!(
            create_sort_name("Attack of the Killer Tomatoes"),
            "attack of killer tomatoes"
        );
        assert_eq!(create_sort_name("All About the"), "all about");
        assert_eq!(create_sort_name("Withnail and I"), "withnail and i");
    }

    /// Words run BEFORE the character sets. With the order reversed the `.`
    /// becomes a space first and `a` then looks like a leading article.
    #[test]
    fn words_are_stripped_before_characters_are_replaced() {
        assert_eq!(
            create_sort_name("A.I. Artificial Intelligence"),
            "a i  artificial intelligence"
        );
        // Removal characters likewise: the leading `-` means C# sees no
        // leading article at all, and only deletes the dash afterwards.
        assert_eq!(create_sort_name("-The Matrix"), "the matrix");
    }

    /// A name that is *only* an article keeps it — C# matches on `"the "`,
    /// `" the "` and `" the"`, none of which occur in a bare `"The"`.
    #[test]
    fn a_bare_article_survives() {
        assert_eq!(create_sort_name("The"), "the");
        assert_eq!(create_sort_name("Theatre"), "theatre");
    }

    // The upstream xUnit oracle for the shared rule
    // (`BaseItemTests.GetSortName_AppliesConfiguredCleaning`).
    #[rstest]
    #[case("The Matrix", "matrix")]
    #[case("Spider-Man", "spiderman")]
    #[case("A Movie: Part 2", "movie: part 0000000002")]
    fn get_sort_name_applies_configured_cleaning(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(get_sort_name(input, true), expected);
    }

    /// `BaseItemTests.GetSortName_WithoutAlphaNumericSorting_ReturnsTrimmedInput`
    /// — the `Person` branch is `TrimStart()` and nothing else.
    #[test]
    fn get_sort_name_without_alpha_numeric_sorting_returns_trimmed_input() {
        assert_eq!(get_sort_name("  The Matrix", false), "The Matrix");
        assert_eq!(get_sort_name("Parity, Alice  ", false), "Parity, Alice  ");
    }

    /// `BaseItemTests.SortName_ForcedSortName_IsCleanedLikeAutoSortName`: a
    /// forced sort name must be cleaned the same way as an auto-generated one
    /// so both sort together (jellyfin#17388) — leading article and hyphen
    /// removed, colon kept, lower-cased.
    #[test]
    fn a_forced_sort_name_is_cleaned_like_an_auto_sort_name() {
        const RAW: &str = "The Spider-Man: Homecoming";
        assert_eq!(forced_sort_key(RAW), create_sort_name(RAW));
        assert_eq!(forced_sort_key(RAW), "spiderman: homecoming");
        // The 10.11.8 short-circuit would have kept the article and the dot.
        assert_eq!(forced_sort_key("The Matrix 2"), "matrix 0000000002");
        assert_eq!(forced_sort_key("Mr. Robot"), "mr  robot");
    }

    /// `ModifySortChunks` closes with `RemoveDiacritics()`, so an accented title
    /// sorts as ASCII and interleaves with the rest of the library instead of
    /// landing after it under SQLite's BINARY collation. Both paths fold: the
    /// derived key and the forced one.
    #[test]
    fn diacritics_are_folded_on_both_paths() {
        assert_eq!(create_sort_name("Café Größe"), "cafe grosse");
        assert_eq!(create_sort_name("Amélie"), "amelie");
        assert_eq!(forced_sort_key("Æon Flux 2"), "aeon flux 0000000002");
    }

    /// The remove set (`, & - { } '`) is deleted outright and each of the
    /// replace set (`. + %`) becomes its own space — C# does not collapse runs.
    #[test]
    fn the_default_character_sets_are_applied_verbatim() {
        assert_eq!(create_sort_name("Mr. & Mrs-Smith"), "mr   mrssmith");
        assert_eq!(create_sort_name("{Braces} 'quoted'"), "braces quoted");
        assert_eq!(create_sort_name("100% Wolf"), "0000000100  wolf");
        assert_eq!(
            create_sort_name("Crosby, Stills + Nash"),
            "crosby stills   nash"
        );
    }

    #[test]
    fn a_blank_name_yields_a_blank_key() {
        assert_eq!(create_sort_name("   "), "");
    }
}
