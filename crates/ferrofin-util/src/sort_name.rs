//! Sort-name derivation — port of C# `BaseItem.GetSortName` / `CreateSortName` /
//! `ModifySortChunks` (Jellyfin 12.0 `MediaBrowser.Controller/Entities/BaseItem.cs`),
//! with the server's `SortRemoveWords` / `SortRemoveCharacters` /
//! `SortReplaceCharacters` ([`SortRules`], installed by the server with
//! [`install_sort_rules`]; Jellyfin's defaults until then).

use std::sync::{Arc, LazyLock, PoisonError, RwLock};

use icu_properties::CodePointMapData;
use icu_properties::props::GeneralCategory;

use crate::string_extensions::{
    lower_invariant, remove_diacritics, remove_diacritics_nfd, transliterated,
};

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
/// `modify_sort_chunks` left-pads every run of digits to 10 so numbers sort
/// naturally (`Movie 0001 (2020)` → `movie 0000000001 (0000002020)`), strips
/// diacritics, and romanises a name that is still not ASCII
/// (`Café 東京` → `cafe dong jing`).
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
    get_sort_name_with(
        name,
        enable_alpha_numeric_sorting,
        lower_invariant,
        rules,
        modify_sort_chunks,
    )
}

/// [`get_sort_name`] as builds before the transliteration step derived it
/// (ASCII digit runs padded, nothing romanised), over the installed rules,
/// kept only so the one-shot sort-name repair can recognise the keys they
/// stored. Never use it for a new write or a query parameter.
#[must_use]
pub fn untransliterated_sort_name(name: &str) -> String {
    get_sort_name_with(
        name,
        true,
        lower_invariant,
        &installed_rules(),
        modify_sort_chunks_untransliterated,
    )
}

/// [`get_sort_name`] as builds between the transliteration step and gl-1908
/// derived it (diacritics folded by [`remove_diacritics_nfd`]), over the
/// installed rules, kept only so the one-shot repair that ports the
/// `Diacritics` package's table can recognise the keys they stored. Never use
/// it for a new write or a query parameter.
#[must_use]
pub fn nfd_folded_sort_name(name: &str) -> String {
    get_sort_name_with(
        name,
        true,
        lower_invariant,
        &installed_rules(),
        modify_sort_chunks_nfd,
    )
}

/// The derivation before invariant casing (full Unicode `to_lowercase`), kept
/// only so migration `0031` can recognise keys Ferrofin wrote with it, always
/// over Jellyfin's default rules (Ferrofin wrote those keys before it read the
/// configured ones). Never use it for a new write or a query parameter.
#[must_use]
pub fn previous_create_sort_name(name: &str) -> String {
    get_sort_name_with(
        name,
        true,
        str::to_lowercase,
        &SortRules::default(),
        modify_sort_chunks_untransliterated,
    )
}

/// [`create_sort_name`] over Jellyfin's default rules, whatever is installed,
/// as it was before the transliteration step: the `ferrofin_sort_name` SQLite
/// function released migrations call, which must keep giving the answer they
/// were written against (and stay deterministic, as the function is
/// registered).
#[must_use]
pub fn default_rules_sort_name(name: &str) -> String {
    get_sort_name_with(
        name,
        true,
        lower_invariant,
        &SortRules::default(),
        modify_sort_chunks_untransliterated,
    )
}

/// [`forced_sort_key`] over Jellyfin's default rules: the
/// `ferrofin_forced_sort_key` SQLite function, frozen for the same reason as
/// [`default_rules_sort_name`].
#[must_use]
pub fn default_rules_forced_sort_key(forced: &str) -> String {
    default_rules_sort_name(forced)
}

/// The 10.11.8-era forced key — `ModifySortChunks(ForcedSortName)` through
/// full Unicode `to_lowercase` — kept only so migration `0031` can recognise
/// stored values written with it.
#[must_use]
pub fn previous_forced_sort_key(forced: &str) -> String {
    modify_sort_chunks_untransliterated(forced).to_lowercase()
}

fn get_sort_name_with(
    name: &str,
    enable_alpha_numeric_sorting: bool,
    lowercase: fn(&str) -> String,
    rules: &SortRules,
    modify_chunks: fn(&str) -> String,
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
    modify_chunks(&sortable)
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

/// Port of `BaseItem.ModifySortChunks`: left-pads each run of digits in
/// `name` to width 10 with `0`, strips diacritics, and when anything is still
/// non-ASCII, romanises the result ([`transliterated`], upstream's
/// `Transliterated()`).
///
/// A digit is C#'s `char.IsDigit` on a UTF-16 unit: any Unicode `Nd`
/// character in the Basic Multilingual Plane (`٣`, `２`), never one outside it,
/// which C# sees as two surrogates.
fn modify_sort_chunks(name: &str) -> String {
    modify_sort_chunks_with(name, remove_diacritics)
}

/// [`modify_sort_chunks`] over `fold`.
fn modify_sort_chunks_with(name: &str, fold: fn(&str) -> String) -> String {
    let categories = CodePointMapData::<GeneralCategory>::new();
    let folded = fold(&pad_digit_runs(name, |c| {
        c.is_ascii_digit()
            || (!c.is_ascii()
                && u32::from(c) <= 0xFFFF
                && categories.get(c) == GeneralCategory::DecimalNumber)
    }));
    if folded.is_ascii() {
        folded
    } else {
        transliterated(&folded)
    }
}

/// [`modify_sort_chunks`] as builds before gl-1908 had it: the diacritic fold
/// was [`remove_diacritics_nfd`]. Kept for the repair that recognises the keys
/// it gave.
fn modify_sort_chunks_nfd(name: &str) -> String {
    modify_sort_chunks_with(name, remove_diacritics_nfd)
}

/// [`modify_sort_chunks`] as builds before the transliteration step had it:
/// ASCII digit runs padded and diacritics stripped with the fold of the time
/// ([`remove_diacritics_nfd`]), nothing romanised. Frozen for the functions
/// released migrations call and the repairs that recognise the keys it gave.
fn modify_sort_chunks_untransliterated(name: &str) -> String {
    remove_diacritics_nfd(&pad_digit_runs(name, |c| c.is_ascii_digit()))
}

/// Left-pads each maximal run of `is_digit` characters in `name` to 10
/// characters with ASCII `0`.
fn pad_digit_runs(name: &str, is_digit: impl Fn(char) -> bool) -> String {
    let mut out = String::with_capacity(name.len());
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        if !is_digit(c) {
            out.push(c);
            continue;
        }
        let mut run = String::from(c);
        while let Some(digit) = chars.next_if(|&next| is_digit(next)) {
            run.push(digit);
        }
        out.extend(std::iter::repeat_n(
            '0',
            10_usize.saturating_sub(run.chars().count()),
        ));
        out.push_str(&run);
    }
    out
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

    /// The oracle: each name created as a collection on a stock Jellyfin 12.4
    /// (`POST /Collections`), its `SortName` read back. Upstream romanises
    /// whatever is still non-ASCII after the diacritic fold
    /// (`ModifySortChunks` → `Transliterated()`), and pads every Unicode `Nd`
    /// digit run (`char.IsDigit`).
    #[rstest]
    #[case("Æon Flux", "aeon flux")]
    #[case("Amélie", "amelie")]
    #[case("霸王别姬", "ba wang bie ji")]
    #[case("Брат", "brat")]
    #[case("Café Ünïcode 東京", "cafe unicode dong jing")]
    #[case("Χάος", "chaos")]
    #[case("ちびまる子ちゃん", "chibimaru zichan")]
    #[case("重庆森林", "chong qing sen lin")]
    #[case("大話西遊", "da hua xi you")]
    #[case("東京 2", "dong jing 0000000002")]
    #[case("東京物語", "dong jing wu yu")]
    #[case("Ευρώπη", "europe")]
    #[case("Ёжик в тумане", "ezik v tumane")]
    #[case("Garðarsson", "gardarsson")]
    #[case("기생충", "gisaengchung")]
    #[case("괴물", "goemul")]
    #[case("ゴジラ", "gojira")]
    #[case("حب في القاهرة", "hb fy alqahrt")]
    #[case("Иван Грозный", "ivan groznyi")]
    #[case("ジョジョの奇妙な冒険", "jojono qi miaona mao xian")]
    #[case("君の名は。", "junno mingha")]
    #[case("カウボーイビバップ", "kauboibibappu")]
    #[case("Kraków ł", "krakow l")]
    #[case("もののけ姫", "mononoke ji")]
    #[case("Μπουμπουλίνα", "mpoumpoulina")]
    #[case("नमस्ते", "namaste")]
    #[case("Οδύσσεια", "odysseia")]
    #[case("오징어 게임", "ojingeo geim")]
    #[case("올드보이", "oldeuboi")]
    #[case("Пётр 1", "petr 0000000001")]
    #[case("七人の侍", "qi renno shi")]
    #[case("千と千尋の神隠し", "qianto qian xunno shen yinshi")]
    #[case("“Quoted” Title", "quoted title")]
    #[case("ラーメン", "ramen")]
    #[case("살인의 추억", "salinui chueog")]
    #[case("Щелкунчик", "selkuncik")]
    #[case("שלום", "slwm")]
    #[case("Spider-Man: 東京!", "spiderman dong jing")]
    #[case("Сталкер", "stalker")]
    #[case("Straße", "strasse")]
    #[case("Тарас Бульба", "taras bulba")]
    #[case("Θεός", "theos")]
    #[case("Þorsteinn", "thorsteinn")]
    #[case("となりのトトロ", "tonarinototoro")]
    #[case("Українська ніч", "ukrainska nic")]
    #[case("Ünïcode — Dash", "unicode  dash")]
    #[case("Ⅷ Legion", "viii legion")]
    #[case("Война и мир", "voina i mir")]
    #[case("ヴァイオレット・エヴァーガーデン", "vu~aiorettoevu~agaden")]
    #[case("長江七號", "zhang jiang qi hao")]
    #[case("Ζορμπάς", "zormpas")]
    #[case("Αλέξης Ζορμπάς", "alexes zormpas")]
    #[case("Ангел-хранитель: Ночь!", "angelhranitel noc")]
    #[case("北京", "bei jing")]
    #[case("ＤＥＡＴＨ ＮＯＴＥ", "death note")]
    #[case("東京ゴッドファーザーズ", "dong jinggoddofazazu")]
    #[case("ガンダム", "gandamu")]
    #[case("한국", "hangug")]
    #[case("Їжак", "izak")]
    #[case("ジョジョ", "jojo")]
    #[case("Movie 🎬", "movie 🎬")]
    #[case("Ω Ζ", "o z")]
    #[case("Ολυμπία", "olympia")]
    #[case("ポケモン", "pokemon")]
    #[case("Ψυχή", "psyche")]
    #[case("日本", "ri ben")]
    #[case("사랑", "salang")]
    #[case("신과함께", "singwahamkke")]
    #[case("ヱヴァンゲリヲン", "wevu~angeriwon")]
    #[case("臥虎藏龍", "wo hu cang long")]
    #[case("卧虎藏龙", "wo hu cang long")]
    #[case("ファーザー東京", "faza dong jing")]
    #[case("東京ファーザー", "dong jingfaza")]
    #[case("Tokyo東京", "tokyo dong jing")]
    #[case("東京Tokyo", "dong jingtokyo")]
    #[case("Москва東京", "moskva dong jing")]
    #[case("ジョッ東京", "jo~tsu dong jing")]
    #[case("東京っ", "dong jing~tsu")]
    #[case("がっこう", "gakkou")]
    #[case("東京２", "dong jing0000000002")]
    #[case("Movie ٣", "movie 0000000003")]
    #[case("सत्या २", "satya 0000000002")]
    #[case("Film 𝟙", "film 𝟙")]
    #[case("ｶﾞﾝﾀﾞﾑ", "gandamu")]
    #[case("東京\u{3000}物語", "dong jing wu yu")]
    #[case("ＲＥＤ２", "red0000000002")]
    #[case("ΟΔΥΣΣΕΑΣ", "odysseas")]
    #[case("Արարատ", "ararat")]
    #[case("თბილისი", "tbilisi")]
    #[case("기생충 2", "gisaengchung 0000000002")]
    #[case("Москва Moscow", "moskva moscow")]
    #[case("東京・物語", "dong jingwu yu")]
    #[case("東京：物語", "dong jing wu yu")]
    #[case("ﬁlm", "film")]
    #[case("ひらがな", "hiragana")]
    #[case("Pokémon ポケモン", "pokemon pokemon")]
    #[case("Đorđe", "dorde")]
    #[case("ıstanbul", "istanbul")]
    #[case("€uro 東京", "€uro dong jing")]
    #[case("Love ♥ 東京", "love ♥ dong jing")]
    #[case("«Брат»", "<<brat>>")]
    #[case("Ærø 東京", "aero dong jing")]
    #[case("Mañana Ψ", "manana ps")]
    #[case("Ⅻ Cyrillic Б", "xii cyrillic b")]
    #[case("Ελλάδα 2024", "ellada 0000002024")]
    #[case("Шрек 2", "srek 0000000002")]
    #[case("鬼滅の刃", "gui mieno ren")]
    #[case("進撃の巨人", "jin jino ju ren")]
    #[case("魔女の宅急便", "mo nuno zhai ji bian")]
    #[case("天気の子", "tian qino zi")]
    #[case("攻殻機動隊", "gong qiao ji dong dui")]
    #[case("龍猫", "long mao")]
    #[case("饕餮", "tao tie")]
    #[case("いすゞ", "isuzu")]
    #[case("こゝろ", "kokoro")]
    #[case("ヶ月", "~ke yue")]
    #[case("ヴィンランド・サガ", "vu~inrandosaga")]
    #[case("シン・ゴジラ", "shingojira")]
    #[case("ワンピース", "wanpisu")]
    #[case("ハウルの動く城", "hauruno dongku cheng")]
    #[case("ポニョ", "ponyo")]
    #[case("ﾄﾄﾛ", "totoro")]
    #[case("마더", "madeo")]
    #[case("아가씨", "agassi")]
    #[case("버닝", "beoning")]
    #[case("헤어질 결심", "heeojil gyeolsim")]
    #[case("ДЖЕНТЛЬМЕНЫ УДАЧИ", "dzentlmeny udaci")]
    #[case("Сергей Бондарчук", "sergei bondarcuk")]
    #[case("Љубав", "lubav")]
    #[case("Ђорђе", "dorde")]
    #[case("Ґрунт", "grunt")]
    #[case("Ўзбек", "uzbek")]
    #[case("Ἰλιάς", "ilias")]
    #[case("Αθηνά", "athena")]
    #[case("Ωραία", "oraia")]
    #[case("Ἀχιλλεύς", "achilleus")]
    #[case("בראשית", "brsyt")]
    #[case("الرسالة", "alrsalt")]
    #[case("दिलवाले दुल्हनिया ले जाएंगे", "dilavale dulhaniya le jaenge")]
    #[case("শোলে", "sole")]
    #[case("தமிழ்", "tamil")]
    #[case("తెలుగు", "telugu")]
    #[case("ಕನ್ನಡ", "kannada")]
    #[case("മലയാളം", "malayalam")]
    #[case("ગુજરાતી", "gujarati")]
    #[case("ਪੰਜਾਬੀ", "pajabi")]
    #[case("ଓଡ଼ିଆ", "oria")]
    #[case("Հայաստան", "hayastan")]
    #[case("საქართველო", "sakartvelo")]
    #[case("ދިވެހި", "divehi")]
    #[case("ܐܪܡܝܐ", "ܐܪܡܝܐ")]
    #[case("ᐃᓄᒃᑎᑐᑦ", "ᐃᓄᒃᑎᑐᑦ")]
    #[case("አማርኛ", "አማርኛ")]
    #[case("ဗမာ", "ဗမာ")]
    #[case("ខ្មែរ", "ខមែរ")]
    #[case("ລາວ", "ລາວ")]
    #[case("བོད་", "བད")]
    #[case("Ελλάδα: 東京 — Москва!", "ellada dong jing  moskva")]
    #[case("Café ½", "cafe  12")]
    #[case("x² Movie", "x² movie")]
    #[case("№ 5", "no 0000000005")]
    #[case("Ⅳ", "iv")]
    #[case("℃ Story", "℃ story")]
    #[case("𝐁𝐨𝐥𝐝", "𝐁𝐨𝐥𝐝")]
    #[case("ⓐⓑⓒ", "ⓐⓑⓒ")]
    #[case("Ｍｏｖｉｅ\u{3000}Ｎｏ．１", "movie no0000000001")]
    #[case("Ǆungla", "dzungla")]
    #[case("Ŋ Œuvre", "n oeuvre")]
    #[case("සිංහල", "සංහල")]
    #[case("ᠮᠣᠩᠭᠣᠯ", "ᠮᠣᠩᠭᠣᠯ")]
    #[case("ᏣᎳᎩ", "ꮳꮃꭹ")]
    #[case("ꆈꌠ", "ꆈꌠ")]
    #[case("ᚠᚢᚦ", "ᚠᚢᚦ")]
    #[case("ⲙⲉⲧⲣⲉⲙ", "ⲙⲉⲧⲣⲉⲙ")]
    #[case("ⰀⰁⰂ", "ⰰⰱⰲ")]
    #[case("𐌲𐌿𐍄", "𐌲𐌿𐍄")]
    #[case("ߒߞߏ", "ߒߞߏ")]
    #[case("ⵜⴰⵎⴰⵣⵉⵖⵜ", "ⵜⴰⵎⴰⵣⵉⵖⵜ")]
    #[case("ꦗꦮ", "ꦗꦮ")]
    #[case("ᬩᬮᬶ", "ᬩᬮ")]
    #[case("ㆍ한글", "ㆍhangeul")]
    #[case("\u{1100}\u{1161}", "ga")]
    #[case("ꓡꓲ", "ꓡꓲ")]
    #[case("ᜊᜌ᜔ᜊᜌᜒᜈ᜔", "ᜊᜌᜊᜌᜈ")]
    fn sort_names_match_jellyfin_12_4(#[case] name: &str, #[case] expected: &str) {
        assert_eq!(create_sort_name(name), expected, "{name}");
    }

    /// Where the same oracle differs, with Jellyfin 12.4's key in the comment.
    /// ICU4X has no Thai-Latin (gl-1907). Its CLDR is newer than ICU 60's
    /// for Bopomofo (no tone number), `Latin-ASCII` (it folds `ꜱ`) and
    /// Hebrew-Latin (it drops the vowel points the diacritic fold now keeps):
    /// gl-2038.
    #[rstest]
    #[case("สวัสดี", "สวสด")] // swasdi
    #[case("ภาพยนตร์", "ภาพยนตร")] // phaphyntr
    #[case("ㄅㄆㄇ", "bpm")] // bpm1
    #[case("ꜱᴍᴀʟʟ ᴄᴀᴘꜱ", "small caps")] // ꜱmall capꜱ
    #[case("שָׁלוֹם", "slwm")] // salwom
    fn sort_names_that_differ_from_jellyfin_12_4(#[case] name: &str, #[case] ours: &str) {
        assert_eq!(create_sort_name(name), ours, "{name}");
    }

    /// Names with combining marks and letters the `Diacritics` table maps or
    /// leaves (gl-1908), against `BaseItem.GetSortName(name, true, new
    /// ServerConfiguration())` run from the assemblies the Jellyfin 12.4 image
    /// ships.
    #[rstest]
    #[case("Zoë Saldaña", "zoe saldana")]
    #[case("Zoe\u{308} Saldan\u{303}a", "zoe saldana")]
    #[case("مُحَمَّد", "muhamad")]
    #[case("Straße", "strasse")]
    #[case("STRA\u{1E9E}E", "strasse")]
    #[case("¡Three Amigos!", "three amigos!")]
    #[case("¿Quién?", "quien?")]
    #[case("Esperanto Ĉeĥo", "esperanto ceho")]
    #[case("Wǒ Ài Nǐ", "wo ai ni")]
    #[case("Țară", "tara")]
    #[case("Ţară", "tara")]
    #[case("Muškatų", "muskatu")]
    #[case("Fran\u{63}\u{327}ois", "francois")]
    #[case("Αθήνα", "athena")]
    #[case("Ἀθῆναι", "athenai")]
    #[case("Андрей", "andrei")]
    #[case("Ёлка", "elka")]
    #[case("ѓуб", "gub")]
    #[case("Mañana ª º", "manana a o")]
    #[case("İstanbul ı", "Istanbul i")]
    #[case("Əli ə", "ali a")]
    #[case("Đorđe", "dorde")]
    #[case("Æon Fløde Œuvre Łódź", "aeon flode oeuvre lodz")]
    #[case("Ðð Þþ", "dd thth")]
    #[case("الموسيقى", "almwsyqy")]
    #[case("ٱلله", "allh")]
    #[case("ஔவை", "ovai")]
    #[case("\u{212B}\u{2126}\u{212A}", "aok")]
    #[case("\u{F900}", "qi")]
    #[case("Tiếng Việt", "tieng viet")]
    #[case("\u{FB2A}לום", "slwm")]
    #[case("किताब", "kitaba")]
    #[case("é\u{301}", "e")]
    #[case("a\u{FFFD}b", "ab")]
    fn sort_names_with_diacritics_match_jellyfin_12_4(#[case] name: &str, #[case] expected: &str) {
        assert_eq!(create_sort_name(name), expected, "{name}");
    }

    #[test]
    fn a_blank_name_yields_a_blank_key() {
        assert_eq!(create_sort_name("   "), "");
    }
}
