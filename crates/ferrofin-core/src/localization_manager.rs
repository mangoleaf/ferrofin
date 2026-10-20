//! [`LocalizationManager`] — a concrete culture/country/parental-rating service.
//!
//! Ports Jellyfin's cultures, country tables, parental ratings and core strings.
//! Immutable embedded tables are parsed once. Server UI culture and metadata
//! country are read from live configuration when resolving a value.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock};

use ferrofin_model::entities_media::{ParentalRating, ParentalRatingScore};
use ferrofin_model::globalization::{CountryInfo, CultureDto, LocalizationOption};

/// The server default culture, used when no country/culture is supplied.
const DEFAULT_METADATA_COUNTRY_CODE: &str = "US";

/// Values that mean "no rating" and resolve to `None` (C# `_unratedValues`).
const UNRATED_VALUES: &[&str] = &["n/a", "unrated", "not rated", "nr"];

/// C# `IsUnrated`: the trimmed value is one of [`UNRATED_VALUES`], ordinal
/// ignore-case.
fn is_unrated(rating: &str) -> bool {
    let rating = rating.trim();
    UNRATED_VALUES
        .iter()
        .any(|u| u.eq_ignore_ascii_case(rating))
}

/// Jellyfin's embedded ISO 639-2 language table (`iso6392.txt`), one language per line,
/// pipe-delimited `iso639-2/T | iso639-2/B | iso639-1 | English name | French name`. Ported
/// verbatim from upstream so `GET /Localization/Cultures` yields the same ~200-language list.
const ISO6392: &str = include_str!("data/iso6392.txt");

/// Jellyfin's bundled country dataset, vendored verbatim from
/// `Emby.Server.Implementations/Localization/countries.json` (v12.2, 140 ISO-3166 entries).
const COUNTRIES_JSON: &str = include_str!("data/countries.json");

#[path = "data/parental_ratings.rs"]
mod parental_ratings;

#[path = "data/core_dictionaries.rs"]
mod core_dictionaries;

/// `LocalizationManager.DefaultCulture`.
const DEFAULT_CULTURE: &str = "en-US";

/// The parsed core dictionaries, keyed by the resource's culture code (e.g.
/// `de`, `pt-BR`, `es_419`), each case-insensitive on the phrase key like the
/// C# `Dictionary<string, string>(StringComparer.OrdinalIgnoreCase)`. Built once
/// per process (`_cultureOnlyDictionaries`).
static CORE_DICTIONARY_DATA: LazyLock<HashMap<&'static str, HashMap<String, String>>> =
    LazyLock::new(|| {
        core_dictionaries::CORE_DICTIONARIES
            .iter()
            .map(|(code, json)| {
                let parsed: HashMap<String, String> = serde_json::from_str(json)
                    .unwrap_or_else(|e| panic!("embedded core dictionary {code}.json: {e}"));
                let lowered = parsed
                    .into_iter()
                    .map(|(k, v)| (k.to_lowercase(), v))
                    .collect();
                (*code, lowered)
            })
            .collect()
    });

/// `_bcp47ToJellyfinMap`: hyphenated BCP-47 spellings of the resources that use
/// an underscore (`ar-SA` → `ar_SA`, `es-419` → `es_419`, …), case-insensitive.
static BCP47_TO_JELLYFIN: LazyLock<HashMap<String, &'static str>> = LazyLock::new(|| {
    core_dictionaries::CORE_DICTIONARIES
        .iter()
        .filter(|(code, _)| code.contains('_'))
        .map(|(code, _)| (code.replace('_', "-").to_lowercase(), *code))
        .collect()
});

/// `GetResourceFilename` minus the `.json`: lower-case language, upper-case
/// region, separator preserved (`pt-br` → `pt-BR`, `ES_419` → `es_419`).
fn normalize_culture_code(culture: &str) -> String {
    match culture.find(['-', '_']) {
        Some(i) if i > 0 => {
            let (lang, rest) = culture.split_at(i);
            let (sep, region) = rest.split_at(1);
            format!("{}{sep}{}", lang.to_lowercase(), region.to_uppercase())
        }
        _ => culture.to_lowercase(),
    }
}

/// `GetLocalizationDictionary(culture)`: the parsed dictionary for a culture
/// code, or `None` when no resource ships for it (the C# logs and falls back).
fn core_dictionary(culture: &str) -> Option<&'static HashMap<String, String>> {
    let file = normalize_culture_code(culture);
    CORE_DICTIONARY_DATA
        .iter()
        .find(|(code, _)| **code == file)
        .map(|(_, dict)| dict)
}

/// Native labels captured from .NET 10 `CultureInfo.NativeName` for the pinned
/// core-resource catalog, including the explicit English label for `en-US`.
const LOCALIZATION_OPTIONS_JSON: &str = include_str!("data/localization_options.json");

/// One country row as stored in `countries.json` (PascalCase keys).
#[derive(serde::Deserialize)]
struct CountryRow {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "DisplayName")]
    display_name: String,
    #[serde(rename = "TwoLetterISORegionName")]
    two: String,
    #[serde(rename = "ThreeLetterISORegionName")]
    three: String,
}

/// The vendored country table, parsed once from `countries.json` and kept in file order.
static COUNTRIES: LazyLock<Vec<CountryInfo>> = LazyLock::new(|| {
    // Port of C# GetCountries: deserialize the bundled countries.json (140 ISO-3166 regions).
    serde_json::from_str::<Vec<CountryRow>>(COUNTRIES_JSON)
        .unwrap_or_default()
        .into_iter()
        .map(|c| CountryInfo {
            name: c.name,
            display_name: c.display_name,
            two_letter_iso_region_name: c.two,
            three_letter_iso_region_name: c.three,
        })
        .collect()
});

/// The dropdown follows the shipped resources, with .NET native labels and
/// ordinal-ignore-case label ordering (`BuildLocalizationData`).
static LOCALIZATION_OPTION_LIST: LazyLock<Vec<LocalizationOption>> = LazyLock::new(|| {
    let labels: Vec<LocalizationOption> = serde_json::from_str(LOCALIZATION_OPTIONS_JSON)
        .expect("embedded localization labels are valid JSON");
    let mut options: Vec<_> = core_dictionaries::CORE_DICTIONARIES
        .iter()
        .filter(|(code, _)| !code.eq_ignore_ascii_case(DEFAULT_CULTURE))
        .map(|(code, _)| LocalizationOption {
            name: labels
                .iter()
                .find(|option| option.value == *code)
                .map_or_else(|| (*code).to_owned(), |option| option.name.clone()),
            value: (*code).to_owned(),
        })
        .collect();
    options.push(LocalizationOption {
        name: "English".to_owned(),
        value: DEFAULT_CULTURE.to_owned(),
    });
    options.sort_by_cached_key(|option| {
        ferrofin_util::string_extensions::upper_invariant(&option.name)
            .encode_utf16()
            .collect::<Vec<_>>()
    });
    options
});

/// The parsed ISO 639-2 dataset, built once.
static CULTURE_DATA: LazyLock<CultureData> = LazyLock::new(load_culture_data);

/// `FindLanguageInfo`'s lookup, indexed once: every non-empty display name,
/// name, two-letter and three-letter code (ASCII-lowercased) → the index of
/// the FIRST culture in file order carrying it, so a hit is exactly the
/// culture the linear `FirstOrDefault` scan would return. Upstream memoises
/// the same answer per token in `_cultureCache`; this keeps the per-item cost
/// on the DTO path at one hash lookup.
static LANGUAGE_INDEX: LazyLock<HashMap<String, usize>> = LazyLock::new(|| {
    let mut index = HashMap::new();
    for (i, culture) in CULTURE_DATA.cultures.iter().enumerate() {
        let keys = [
            culture.display_name.as_str(),
            culture.name.as_str(),
            culture.two_letter_iso_language_name.as_str(),
        ]
        .into_iter()
        .chain(
            culture
                .three_letter_iso_language_names
                .iter()
                .map(String::as_str),
        );
        for key in keys.filter(|key| !key.is_empty()) {
            index.entry(key.to_ascii_lowercase()).or_insert(i);
        }
    }
    index
});

/// C# `FindLanguageInfo` over the static culture table: the first culture
/// whose display name, name, two-letter or any three-letter code equals
/// `language` ignoring ASCII case; `None` for an empty or unknown token.
#[must_use]
pub fn find_culture(language: &str) -> Option<&'static CultureDto> {
    if language.is_empty() {
        return None;
    }
    LANGUAGE_INDEX
        .get(&language.to_ascii_lowercase())
        .map(|&i| &CULTURE_DATA.cultures[i])
}

/// The `BaseItem.OriginalLanguage` setter (`BaseItem.cs:228-232`):
/// `FindLanguageInfo(value)?.TwoLetterISOLanguageName ?? value`. A known
/// token (`"English"`, `"eng"`, `"EN"`) becomes its two-letter code; an
/// unknown one is kept as-is. An ISO 639-2-only culture (`"und"`, `"mul"`)
/// has an empty two-letter code, which C#'s `??` does not fall back from, so
/// it becomes `""` — upstream's behaviour since Jellyfin 12 kept those rows.
#[must_use]
pub fn normalize_original_language(value: &str) -> String {
    find_culture(value).map_or_else(
        || value.to_owned(),
        |culture| culture.two_letter_iso_language_name.clone(),
    )
}

/// Every known rating system, keyed by upper-case country code, parsed once.
static PARENTAL_RATINGS: LazyLock<BTreeMap<String, RatingTable>> = LazyLock::new(|| {
    parental_ratings::TABLES
        .iter()
        .map(|(country, json)| ((*country).to_owned(), load_rating_table(json)))
        .collect()
});

/// A parental-rating system as stored in `Ratings/*.json` (C# `ParentalRatingSystem`).
#[derive(serde::Deserialize)]
struct ParentalRatingSystem {
    #[serde(rename = "ratings", default)]
    ratings: Vec<ParentalRatingEntry>,
}

/// One entry: several rating strings sharing one score (C# `ParentalRatingEntry`).
#[derive(serde::Deserialize)]
struct ParentalRatingEntry {
    #[serde(rename = "ratingStrings", default)]
    rating_strings: Vec<String>,
    #[serde(rename = "ratingScore")]
    rating_score: Option<ParentalRatingScore>,
}

/// The parsed `iso6392.txt` dataset (C# `LoadCultures`' two outputs).
#[derive(Debug)]
struct CultureData {
    /// The culture list, in file order.
    cultures: Vec<CultureDto>,
    /// ISO 639-2/B → /T (C# `_iso6392BtoT`).
    iso6392_b_to_t: HashMap<String, String>,
}

/// One country's parental-rating table.
///
/// The entries are held twice on purpose. `ordered` preserves the vendored file
/// order — which is what C#'s `Dictionary` enumerates, since it never removes keys —
/// so `GET /Localization/ParentalRatings` is deterministic instead of inheriting a
/// `HashMap`'s per-process random iteration order. `by_name` is the lookup index used
/// to resolve a rating string to a score.
///
/// The two views always agree on a rating string's score, including for a repeated
/// one: see [`load_rating_table`].
#[derive(Debug)]
struct RatingTable {
    /// `(rating string, score)` in vendored-file order, one entry per distinct
    /// rating string.
    ordered: Vec<(String, ParentalRatingScore)>,
    /// Rating string → score. Holds exactly the same pairs as `ordered`.
    by_name: HashMap<String, ParentalRatingScore>,
}

/// Culture, country and parental-rating services over the embedded resources.
#[derive(Clone)]
pub struct LocalizationManager {
    /// The server default country code for rating fallback.
    metadata_country_code: Arc<dyn Fn() -> String + Send + Sync>,
    /// `ServerConfiguration.UICulture` — the culture `GetLocalizedString`
    /// resolves phrases in (en-US fallback), read live so an admin change
    /// applies without a restart, as the C# config fallback does.
    ui_culture: Arc<dyn Fn() -> String + Send + Sync>,
}

impl Default for LocalizationManager {
    fn default() -> Self {
        Self::new(DEFAULT_METADATA_COUNTRY_CODE)
    }
}

impl std::fmt::Debug for LocalizationManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalizationManager")
            .field("metadata_country_code", &(self.metadata_country_code)())
            .field("ui_culture", &(self.ui_culture)())
            .finish_non_exhaustive()
    }
}

impl LocalizationManager {
    /// Builds the manager over the embedded dataset, using `metadata_country_code`
    /// (e.g. `"US"`) as the default for rating lookups.
    #[must_use]
    pub fn new(metadata_country_code: &str) -> Self {
        let metadata_country_code = metadata_country_code.to_ascii_uppercase();
        Self {
            metadata_country_code: Arc::new(move || metadata_country_code.clone()),
            ui_culture: Arc::new(|| DEFAULT_CULTURE.to_owned()),
        }
    }

    /// Reads the current metadata country when resolving default ratings.
    #[must_use]
    pub fn with_metadata_country_source(
        mut self,
        source: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        self.metadata_country_code = Arc::new(source);
        self
    }

    /// Sets a fixed server UI culture (`ServerConfiguration.UICulture`) phrases
    /// are localized in; empty keeps the default `en-US`.
    #[must_use]
    pub fn with_ui_culture(self, ui_culture: &str) -> Self {
        let culture = if ui_culture.is_empty() {
            DEFAULT_CULTURE.to_owned()
        } else {
            ui_culture.to_owned()
        };
        self.with_ui_culture_source(move || culture.clone())
    }

    /// Sets a live source for the server UI culture — the composition root
    /// passes a reader over the configuration snapshot so `UICulture` edits
    /// take effect immediately. An empty value means `en-US`.
    #[must_use]
    pub fn with_ui_culture_source(
        mut self,
        source: impl Fn() -> String + Send + Sync + 'static,
    ) -> Self {
        self.ui_culture = Arc::new(source);
        self
    }

    /// Port of `GetLocalizedString(phrase)`: the phrase in the *ambient* UI
    /// culture (`CultureInfo.CurrentUICulture` — the request's, or that of the
    /// work a request started), falling back to `en-US`, then to the phrase key
    /// itself. Outside any request there is no ambient culture, and the lookup
    /// uses the configured server culture like an empty `CurrentUICulture.Name`.
    #[must_use]
    pub fn get_localized_string(&self, phrase: &str) -> String {
        let ambient = ferrofin_util::current_culture::ambient_ui_culture().unwrap_or_default();
        self.get_localized_string_for(phrase, &ambient)
    }

    /// Port of `GetServerLocalizedString(phrase)`: the phrase in the configured
    /// server UI culture, whatever culture the calling request or worker runs
    /// under (library view, collection and extra names).
    #[must_use]
    pub fn get_server_localized_string(&self, phrase: &str) -> String {
        self.get_localized_string_for(phrase, &(self.ui_culture)())
    }

    /// Port of `GetLocalizedString(phrase, culture)`: an empty culture means the
    /// server UI culture; a BCP-47 spelling of an underscore resource maps onto
    /// it (`ar-SA` → `ar_SA`); phrase lookup is case-insensitive; a miss in the
    /// culture falls back to `en-US`, and a miss there returns the phrase.
    #[must_use]
    pub fn get_localized_string_for(&self, phrase: &str, culture: &str) -> String {
        let server_culture;
        let culture = if culture.is_empty() {
            server_culture = (self.ui_culture)();
            if server_culture.is_empty() {
                DEFAULT_CULTURE
            } else {
                server_culture.as_str()
            }
        } else {
            culture
        };
        let culture = BCP47_TO_JELLYFIN
            .get(&culture.to_lowercase())
            .copied()
            .unwrap_or(culture);
        let key = phrase.to_lowercase();
        if let Some(value) = core_dictionary(culture).and_then(|d| d.get(&key)) {
            return value.clone();
        }
        if !culture.eq_ignore_ascii_case(DEFAULT_CULTURE)
            && let Some(value) = core_dictionary(DEFAULT_CULTURE).and_then(|d| d.get(&key))
        {
            return value.clone();
        }
        phrase.to_owned()
    }

    /// All known cultures (C# `GetCultures`).
    #[must_use]
    pub fn get_cultures(&self) -> &[CultureDto] {
        &CULTURE_DATA.cultures
    }

    /// All known countries (C# `GetCountries`).
    #[must_use]
    pub fn get_countries(&self) -> Vec<CountryInfo> {
        COUNTRIES.clone()
    }

    /// The available UI-language options, derived from bundled core resources
    /// with native labels and .NET ordinal-ignore-case label ordering.
    #[must_use]
    pub fn get_localization_options(&self) -> Vec<LocalizationOption> {
        LOCALIZATION_OPTION_LIST.clone()
    }

    /// Finds the culture matching a language token by display name, name,
    /// three-letter code, or two-letter code (C# `FindLanguageInfo`).
    #[must_use]
    pub fn find_language_info(&self, language: &str) -> Option<&CultureDto> {
        find_culture(language)
    }

    /// The display name for a language token, truncated at the first `;`/`,`
    /// (C# `GetLanguageDisplayName`).
    #[must_use]
    pub fn get_language_display_name(&self, language: &str) -> Option<String> {
        if language.is_empty() {
            return None;
        }
        let display = &self.find_language_info(language)?.display_name;
        Some(
            display
                .split([';', ','])
                .next()
                .unwrap_or(display)
                .trim()
                .to_owned(),
        )
    }

    /// The ISO 639-2/T code for a given /B code, if the mapping is known
    /// (C# `TryGetISO6392TFromB`).
    #[must_use]
    pub fn try_get_iso6392_t_from_b(&self, iso_b: &str) -> Option<String> {
        // Keys are stored lower-cased (C#'s dictionary is case-insensitive).
        // Stored languages are almost always lower-case already, so this runs
        // on every stream of every DTO read without allocating for them.
        let map = &CULTURE_DATA.iso6392_b_to_t;
        let found = if iso_b.bytes().any(|b| b.is_ascii_uppercase()) {
            map.get(&iso_b.to_ascii_lowercase())
        } else {
            map.get(iso_b)
        };
        found.filter(|t| !t.is_empty()).cloned()
    }

    /// The parental-rating list for the server default country, back-filled with
    /// the common ratings and ordered by score (C# `GetParentalRatings`).
    ///
    /// Resolves the live server country against the immutable parsed tables.
    #[must_use]
    pub fn get_parental_ratings(&self) -> Vec<ParentalRating> {
        build_parental_ratings(&(self.metadata_country_code)())
    }

    /// Resolves a rating string to a score (C# `GetRatingScore`, Jellyfin 12.2
    /// `LocalizationManager.cs:374-430`).
    ///
    /// An unrated marker is checked on the whole value first (some contain a
    /// `/` themselves: `n/a`), then the whole value is looked up before any
    /// splitting, because several systems keep a `/` inside one rating
    /// (`M/12`, `U/A 13+`, `7/i/fig`). Only a value with more than one
    /// non-empty `/`-part falls back to the per-part lookup, where an unrated
    /// part is skipped rather than ending the search and the first part that
    /// resolves wins (`FR:16 / US:12` → 16). Accepted divergence: a value
    /// whose only non-empty part is not the whole value (`PG/`, ` / PG`) has
    /// that part looked up — upstream (since 12.1) answers unrated, which
    /// would loosen parental filtering; a bare unrated marker part (`NR/`) stays
    /// unrated (a `Rated `-prefixed one, `Rated NR/`, scores as the whole
    /// value `Rated NR` does).
    ///
    /// The per-part lookups stay silent; when none of them resolves, one
    /// message names the whole list. Upstream logs it at warning, but it fires
    /// once per scanned item carrying such a value, so it is `debug` here
    /// (`docs/conventions/LOGGING.md`).
    #[must_use]
    pub fn get_rating_score(
        &self,
        rating: &str,
        country_code: Option<&str>,
    ) -> Option<ParentalRatingScore> {
        if rating.is_empty() || is_unrated(rating) {
            return None;
        }
        if let Some(score) = self.single_rating_score(rating, country_code, true) {
            return Some(score);
        }
        // `Split('/', RemoveEmptyEntries | TrimEntries)`.
        let parts: Vec<&str> = rating
            .split('/')
            .map(str::trim)
            .filter(|part| !part.is_empty())
            .collect();
        // Upstream returns null whenever one part survives the split. Its
        // intent is "the value is that single part, already looked up", which
        // only holds when nothing was dropped. For `PG/`, ` / PG` or `US:PG/`,
        // dropping an empty entry leaves a part that was never looked up, so
        // 12.1 and 12.2 rate them unrated (rc7, and Ferrofin before, gave
        // `PG` = 10).
        // Accepted divergence, "don't port Jellyfin bugs": an unrated score
        // loosens parental filtering, so the surviving part is looked up.
        if let [only] = parts.as_slice() {
            // An unrated marker stays unrated, as in the list loop below.
            if *only == rating.trim() || is_unrated(only) {
                return None;
            }
            return self.single_rating_score(only, country_code, true);
        }
        let mut has_unknown_rating = false;
        for part in parts {
            // One entry of such a list may be unrated while a later one still resolves.
            if is_unrated(part) {
                continue;
            }
            // An unknown entry is not worth a message of its own as long as
            // another entry still resolves.
            if let Some(score) = self.single_rating_score(part, country_code, false) {
                return Some(score);
            }
            has_unknown_rating = true;
        }
        if has_unknown_rating {
            tracing::debug!(
                rating,
                "none of the ratings in the list are in a known rating system; treating as unrated"
            );
        }
        None
    }

    /// Resolves a single rating value — C# `GetSingleRatingScore`
    /// (`LocalizationManager.cs:441-507`). `log_unknown` is upstream's
    /// `logUnknownRating`: `false` for one entry of a `/`-list, whose caller
    /// reports the list as a whole instead.
    fn single_rating_score(
        &self,
        rating: &str,
        country_code: Option<&str>,
        log_unknown: bool,
    ) -> Option<ParentalRatingScore> {
        // Convert ints directly. This may override some of the locale
        // specific age ratings (but those always map to the same age).
        if let Some(age) = parse_rating_as_score(rating) {
            return Some(ParentalRatingScore::new(age, None));
        }

        // Fairly common for some users to have "Rated R" in their rating
        // field: every occurrence of the three spellings is removed, case
        // insensitively (`LocalizationManager.cs:412-415` chains three
        // `Replace(…, OrdinalIgnoreCase)` calls, so `rated: PG-13` cleans too).
        let cleaned = ["Rated :", "Rated:", "Rated "]
            .iter()
            .fold(rating.to_owned(), |acc, prefix| {
                remove_ignore_ascii_case(&acc, prefix)
            });
        let cleaned = cleaned.trim();

        // Use the rating system matching the language.
        if let Some(cc) = country_code.filter(|c| !c.is_empty()) {
            if let Some(table) = parental_ratings_for(cc) {
                if let Some(score) = table.by_name.get(&rating_key(cleaned)) {
                    return Some(*score);
                }
                // "US-PG-13" / "US:PG-13" with the country already known.
                if cleaned.len() > cc.len()
                    && cleaned
                        .get(..cc.len())
                        .is_some_and(|head| head.eq_ignore_ascii_case(cc))
                    && matches!(cleaned.as_bytes()[cc.len()], b'-' | b':')
                    && let Some(score) = cleaned
                        .get(cc.len() + 1..)
                        .and_then(|tail| table.by_name.get(&rating_key(tail.trim())))
                {
                    return Some(*score);
                }
            }
        } else if let Some(table) = parental_ratings_for(&(self.metadata_country_code)())
            && let Some(score) = table.by_name.get(&rating_key(cleaned))
        {
            // Fall back to the server default language for the ratings check.
            return Some(*score);
        }

        // If we don't find anything, check all rating systems, starting with US.
        if let Some(score) =
            parental_ratings_for("us").and_then(|t| t.by_name.get(&rating_key(cleaned)))
        {
            return Some(*score);
        }
        for table in PARENTAL_RATINGS.values() {
            if let Some(score) = table.by_name.get(&rating_key(cleaned)) {
                return Some(*score);
            }
        }

        // A '/' marks a list of ratings, which this would split into the first
        // entry's country and the last entry's rating (upstream 12.2,
        // `LocalizationManager.cs:497-504`); `get_rating_score` splits it first.
        if cleaned.contains('/') {
            return None;
        }

        // Try splitting by a country prefix separator to handle "US:PG-13",
        // "Germany: FSK-18", "DE-FSK-18".
        for separator in [':', '-'] {
            if let SeparatorLookup::Resolved(score) =
                self.rating_score_by_separator(cleaned, separator, log_unknown)
            {
                return score;
            }
        }
        None
    }

    /// C# `TryGetRatingScoreBySeparator` (`LocalizationManager.cs:509-577`):
    /// [`SeparatorLookup::NotApplicable`] when the separator does not apply
    /// (no separator, or nothing after it), otherwise the resolved score —
    /// `Resolved(None)` when the country was identified but its table knows
    /// neither the rating nor a number, which is logged (when `log_unknown`)
    /// and treated as unrated. An explicitly unrated part (`IT-NR`) is
    /// unrated without a message.
    ///
    /// **Accepted divergence (owner decision D4: "don't port Jellyfin bugs").**
    /// This splits at the *first* separator on both sides. Upstream takes the
    /// country before the first separator but the rating after the **last**
    /// one (`LeftPart`/`RightPart`, `StringExtensions.cs:89,106` — `RightPart`
    /// uses `LastIndexOf`), so it misreads every rating that itself contains
    /// the separator: `CA-TV-MA` is unrated upstream (`MA` is not a Canadian
    /// rating) but 18/1 here, and `US-PG-13` resolves only by its number
    /// upstream (13/none) but as 13/0 here. Where the two differ, this answer
    /// is the country table's own entry for the rating.
    fn rating_score_by_separator(
        &self,
        rating: &str,
        separator: char,
        log_unknown: bool,
    ) -> SeparatorLookup {
        let Some((country_part, rating_part)) = rating.split_once(separator) else {
            return SeparatorLookup::NotApplicable;
        };
        let (country_part, rating_part) = (country_part.trim(), rating_part.trim());
        if rating_part.is_empty() {
            return SeparatorLookup::NotApplicable;
        }
        let resolved_country_code = if parental_ratings_for(country_part).is_some() {
            Some(country_part.to_owned())
        } else {
            self.find_language_info(country_part)
                .map(|culture| culture.two_letter_iso_language_name.clone())
        };
        if let Some(cc) = resolved_country_code.as_deref()
            && let Some(table) = parental_ratings_for(cc)
        {
            if let Some(score) = table.by_name.get(&rating_key(rating_part)) {
                return SeparatorLookup::Resolved(Some(*score));
            }
            // Not a recognised rating string: fall back to using the number.
            if let Some(age) = parse_rating_as_score(rating_part) {
                return SeparatorLookup::Resolved(Some(ParentalRatingScore::new(age, None)));
            }
            // Explicitly unrated content (`IT-NR`) is unrated by definition,
            // not a lookup failure.
            if log_unknown && !is_unrated(rating_part) {
                // Upstream logs this at warning; it fires once per rating lookup
                // (every scanned item), so it stays at debug here (LOGGING.md).
                tracing::debug!(
                    rating,
                    country_code = cc,
                    "rating not found in the country's rating system; treating as unrated"
                );
            }
            return SeparatorLookup::Resolved(None);
        }
        // Country not identified or no rating data available: recursive lookup.
        SeparatorLookup::Resolved(
            self.get_rating_score(rating_part, resolved_country_code.as_deref()),
        )
    }
}

/// The outcome of one `TryGetRatingScoreBySeparator` attempt — C#'s
/// `bool` return plus its `out` score, which distinguishes "this separator
/// does not apply, try the next" from "resolved (possibly to unrated)".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SeparatorLookup {
    /// No separator in the string, or nothing after it: try the next one.
    NotApplicable,
    /// The separator applied; this is the answer, `None` meaning unrated.
    Resolved(Option<ParentalRatingScore>),
}

/// Removes every occurrence of `needle` from `haystack`, comparing ASCII
/// case-insensitively — C# `string.Replace(needle, "", OrdinalIgnoreCase)`.
fn remove_ignore_ascii_case(haystack: &str, needle: &str) -> String {
    let mut out = String::with_capacity(haystack.len());
    let mut rest = haystack;
    while !rest.is_empty() {
        let Some(at) = rest
            .char_indices()
            .find(|&(i, _)| {
                rest.get(i..i + needle.len())
                    .is_some_and(|slice| slice.eq_ignore_ascii_case(needle))
            })
            .map(|(i, _)| i)
        else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..at]);
        rest = &rest[at + needle.len()..];
    }
    out
}

/// Parses a rating as a number, allowing a trailing `+` (`16+`) or a leading
/// `-` (C# `TryParseRatingAsScore`, Jellyfin 12.2 `LocalizationManager.cs:586-591`).
///
/// A leading `-` marks a minimum age — the French CNC's `-12` is "not for
/// under 12s" — never a negative score, which would sort the item below every
/// user's parental limit. Surrounding whitespace is allowed on both sides of
/// the stripped `-`/`+`. Accepted divergence from C#, which strips on the
/// untrimmed value and then lets `int.TryParse` read a sign: there ` -12` is
/// −12 (a negative score that passes every limit) and `12+ ` does not parse;
/// here they are 12 and 12.
fn parse_rating_as_score(rating: &str) -> Option<i32> {
    rating
        .trim()
        .trim_start_matches('-')
        .trim_end_matches('+')
        .trim()
        .parse::<i32>()
        .ok()
}

/// The rating table for a country code (case-insensitive).
fn parental_ratings_for(country_code: &str) -> Option<&'static RatingTable> {
    PARENTAL_RATINGS.get(&country_code.to_ascii_uppercase())
}

/// Port of C# `LoadCultures`: parses iso6392.txt (`T|B|1|EnglishName|FrenchName`) into
/// the culture list and the ISO 639-2/B → /T index, in file order.
#[allow(clippy::similar_names)] // iso639_2t / iso639_2b are the standard ISO 639-2 column names
fn load_culture_data() -> CultureData {
    let mut cultures = Vec::new();
    let mut iso6392_b_to_t = HashMap::new();
    for line in ISO6392.lines() {
        let mut cols = line.split('|');
        let iso639_2t = cols.next().unwrap_or("");
        let iso639_2b = cols.next().unwrap_or("");
        let iso639_1 = cols.next().unwrap_or("");
        let display_name = cols.next().unwrap_or("");
        // C# skips a row only when the English name is blank. Since Jellyfin 12
        // (upstream 553f38a237) a row with no ISO 639-1 code is kept with an empty
        // two-letter code, so ISO 639-2-only languages (`und`, `mul`, `zxx`, …)
        // resolve too: all 496 rows of the vendored table.
        if display_name.trim().is_empty() {
            continue;
        }
        let iso639_1 = if iso639_1.trim().is_empty() {
            ""
        } else {
            iso639_1
        };
        // Name = the two-letter code when it is a region tag (contains '-'), else the
        // English display name (C# `twoCharName.Contains('-') ? twoCharName : displayName`).
        let name = if iso639_1.contains('-') {
            iso639_1
        } else {
            display_name
        };
        let three_letter_names = if iso639_2b.is_empty() {
            vec![iso639_2t.to_owned()]
        } else {
            // Record the B→T mapping (C# `iso6392BtoTdict.TryAdd`).
            iso6392_b_to_t
                .entry(iso639_2b.to_ascii_lowercase())
                .or_insert_with(|| iso639_2t.to_owned());
            vec![iso639_2t.to_owned(), iso639_2b.to_owned()]
        };
        cultures.push(CultureDto {
            name: name.to_owned(),
            display_name: display_name.to_owned(),
            two_letter_iso_language_name: iso639_1.to_owned(),
            three_letter_iso_language_name: three_letter_names.first().cloned(),
            three_letter_iso_language_names: three_letter_names,
        });
    }
    CultureData {
        cultures,
        iso6392_b_to_t,
    }
}

/// Parses a vendored `Ratings/*.json` system, expanding each entry's `ratingStrings`
/// to share its score+subScore (C# `LoadAll` ratings loop) and keeping file order.
///
/// A repeated rating string follows C#'s `dict[ratingString] = ratingEntry.RatingScore`
/// exactly: the indexer overwrites the value of the *existing* entry, so the last score
/// wins while the enumeration position stays at the first occurrence. Both views of the
/// table are updated together, which is what keeps `/Localization/ParentalRatings`
/// (built from `ordered`) and `get_rating_score` (which reads `by_name`) reporting the
/// same score for the same rating. The vendored data has no repeated rating string, so
/// this branch is unreachable today and the emitted order is unchanged either way.
/// The `by_name` key for a rating string: .NET invariant uppercase, so the
/// table behaves like `Dictionary<…>(StringComparer.OrdinalIgnoreCase)`.
fn rating_key(rating: &str) -> String {
    ferrofin_util::string_extensions::upper_invariant(rating)
}

fn load_rating_table(json: &str) -> RatingTable {
    let mut ordered: Vec<(String, ParentalRatingScore)> = Vec::new();
    let mut by_name = HashMap::new();
    if let Ok(system) = serde_json::from_str::<ParentalRatingSystem>(json) {
        for entry in system.ratings {
            if let Some(score) = entry.rating_score {
                for rating in entry.rating_strings {
                    // 12.1: rating strings compare case-insensitively —
                    // providers are not consistent (`VM18` vs `vm18`).
                    if by_name.insert(rating_key(&rating), score).is_none() {
                        ordered.push((rating, score));
                    } else if let Some(slot) = ordered
                        .iter_mut()
                        .find(|(name, _)| rating_key(name) == rating_key(&rating))
                    {
                        slot.1 = score;
                    }
                }
            }
        }
    }
    RatingTable { ordered, by_name }
}

/// Port of C# `GetParentalRatings`: the given country's rating table, back-filled with
/// the common ratings and ordered by (score, sub-score).
fn build_parental_ratings(metadata_country_code: &str) -> Vec<ParentalRating> {
    build_parental_ratings_from(parental_ratings_for(metadata_country_code))
}

/// The body of [`build_parental_ratings`], over an already-resolved table, so tests can
/// drive it with a synthetic rating system. `None` means the country has no vendored
/// table and only the back-fill applies.
fn build_parental_ratings_from(table: Option<&RatingTable>) -> Vec<ParentalRating> {
    let mut ratings: Vec<ParentalRating> = table
        .map(|table| {
            table
                .ordered
                .iter()
                .map(|(name, score)| ParentalRating::new(name.clone(), Some(*score)))
                .collect()
        })
        .unwrap_or_default();

    let has_score = |ratings: &[ParentalRating], target: i32| {
        ratings
            .iter()
            .any(|r| r.rating_score.map(|s| s.score) == Some(target))
    };

    // Common ratings back-fill, mirroring the C# additions.
    if !ratings.iter().any(|r| r.name == "Unrated") {
        ratings.push(ParentalRating::new("Unrated".to_owned(), None));
    }
    for (name, score) in [("Approved", 0), ("10", 10), ("13", 13), ("14", 14)] {
        if !has_score(&ratings, score) {
            ratings.push(ParentalRating::new(
                name.to_owned(),
                Some(ParentalRatingScore::new(score, None)),
            ));
        }
    }
    if !ratings
        .iter()
        .any(|r| r.rating_score.map_or(0, |s| s.score) >= 21)
    {
        ratings.push(ParentalRating::new(
            "21".to_owned(),
            Some(ParentalRatingScore::new(21, None)),
        ));
    }
    if !has_score(&ratings, 1000) {
        ratings.push(ParentalRating::new(
            "XXX".to_owned(),
            Some(ParentalRatingScore::new(1000, None)),
        ));
    }
    if !has_score(&ratings, 1001) {
        ratings.push(ParentalRating::new(
            "Banned".to_owned(),
            Some(ParentalRatingScore::new(1001, None)),
        ));
    }

    // Stable sort, so entries sharing a (score, sub-score) keep the vendored file order.
    ratings.sort_by(|a, b| {
        let sa = a.rating_score.map_or(i32::MIN, |s| s.score);
        let sb = b.rating_score.map_or(i32::MIN, |s| s.score);
        sa.cmp(&sb).then_with(|| {
            let ta = a.rating_score.and_then(|s| s.sub_score);
            let tb = b.rating_score.and_then(|s| s.sub_score);
            ta.cmp(&tb)
        })
    });
    ratings
}

/// An item's parental rating score — `BaseItem.GetParentalRatingScore`
/// (`BaseItem.cs:1916-1931`) over the item's OWN ratings: its custom rating
/// when it has one, else its official rating, scored by the localization
/// tables against the server's metadata country; `None` when it has neither
/// or the rating is unknown.
///
/// TODO(parity, open work item): upstream reads `CustomRatingForComparison`
/// and `OfficialRatingForComparison`, which fall back to the item's
/// `DisplayParent` (an episode or season with no rating of its own takes its
/// series'), and scores against `GetPreferredMetadataCountryCode()` (the
/// item's, else its library's, else the server's country). Here only the
/// item's own ratings count and the server's country is used. Un-defer path:
/// pass the display parent's ratings (one batched read of the series per
/// scan pass, as the series-key read) and the library's preferred country
/// into this function, and apply it in the scan, the owned-video copy and
/// the boot repair (`repair_rating_levels`) together, so no writer disagrees
/// with another.
#[must_use]
pub fn parental_rating_score(
    item: &ferrofin_db::entities::base_items::BaseItemEntity,
    localization: &dyn ferrofin_traits::localization::LocalizationManager,
) -> Option<ParentalRatingScore> {
    let rating = item
        .custom_rating
        .as_deref()
        .filter(|r| !r.is_empty())
        .or_else(|| item.official_rating.as_deref().filter(|r| !r.is_empty()))?;
    // `GetRatingScore(rating, GetPreferredMetadataCountryCode())`.
    localization.get_rating_score(rating, item.preferred_metadata_country_code.as_deref())
}

impl ferrofin_traits::localization::LocalizationManager for LocalizationManager {
    fn get_cultures(&self) -> Vec<CultureDto> {
        CULTURE_DATA.cultures.clone()
    }

    fn get_countries(&self) -> Vec<CountryInfo> {
        LocalizationManager::get_countries(self)
    }

    fn get_parental_ratings(&self) -> Vec<ParentalRating> {
        LocalizationManager::get_parental_ratings(self)
    }

    fn get_localization_options(&self) -> Vec<LocalizationOption> {
        LocalizationManager::get_localization_options(self)
    }

    fn get_supported_ui_cultures(&self) -> Vec<String> {
        // Pinned BuildLocalizationData scans all embedded Core resources, not
        // the historical fixed dropdown. Request options validate the names.
        core_dictionaries::CORE_DICTIONARIES
            .iter()
            .map(|(code, _)| code.replace('_', "-"))
            .collect()
    }

    fn get_localized_string(&self, phrase: &str) -> String {
        LocalizationManager::get_localized_string(self, phrase)
    }

    fn get_server_localized_string(&self, phrase: &str) -> String {
        LocalizationManager::get_server_localized_string(self, phrase)
    }

    fn get_localized_string_for(&self, phrase: &str, culture: &str) -> String {
        LocalizationManager::get_localized_string_for(self, phrase, culture)
    }

    fn find_language_info(&self, language: &str) -> Option<CultureDto> {
        LocalizationManager::find_language_info(self, language).cloned()
    }

    fn get_language_display_name(&self, language: &str) -> Option<String> {
        LocalizationManager::get_language_display_name(self, language)
    }

    fn try_get_iso6392_t_from_b(&self, iso_b: &str) -> Option<String> {
        LocalizationManager::try_get_iso6392_t_from_b(self, iso_b)
    }

    fn get_rating_score(
        &self,
        rating: &str,
        country_code: Option<&str>,
    ) -> Option<ParentalRatingScore> {
        LocalizationManager::get_rating_score(self, rating, country_code)
    }
}

/// The language-token lookup the naming crate's `ExternalPathParser` needs
/// to turn a sidecar's `.en`/`.English`/`.eng` token into a culture — the
/// same `FindLanguageInfo` upstream injects as `ILocalizationManager`.
impl ferrofin_naming::external_files::LocalizationManager for LocalizationManager {
    fn find_language_info(&self, language: &str) -> Option<CultureDto> {
        LocalizationManager::find_language_info(self, language).cloned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use ferrofin_traits::localization::LocalizationManager as LocalizationManagerTrait;
    use rstest::rstest;

    use super::*;

    /// `GetLocalizedString(phrase)` reads the ambient `CurrentUICulture`;
    /// `GetServerLocalizedString` the configured `UICulture`. A request in French
    /// on a German-configured server sees French stream labels but German
    /// library-view names, work it spawns keeps both, and outside any request
    /// the ambient lookup is the configured culture.
    #[tokio::test]
    async fn ambient_and_server_localized_strings_use_their_own_cultures() {
        use ferrofin_util::current_culture::{scope, scope_with_ui};
        let server = LocalizationManager::default().with_ui_culture("de");
        assert_eq!(server.get_localized_string("Default"), "Standard");
        let request = scope_with_ui("fr".into(), Some("fr".into()), async {
            let spawned = scope(ferrofin_util::current_culture::capture(), async {
                (
                    server.get_localized_string("Default"),
                    server.get_server_localized_string("Default"),
                )
            })
            .await;
            (
                server.get_localized_string("Default"),
                server.get_server_localized_string("Default"),
                spawned,
            )
        })
        .await;
        assert_eq!(request.0, "Par défaut");
        assert_eq!(request.1, "Standard");
        assert_eq!(request.2, ("Par défaut".to_owned(), "Standard".to_owned()));
        assert_eq!(server.get_localized_string("Default"), "Standard");
    }

    /// `GetLocalizedString`: the server UI culture, BCP-47 → resource mapping,
    /// case-insensitive phrase keys, en-US fallback, then the phrase itself.
    /// Expected values are the vendored `Core/*.json` entries.
    #[test]
    fn localized_strings_follow_the_c_sharp_lookup_rules() {
        let en = LocalizationManager::default();
        assert_eq!(
            en.get_localized_string("HearingImpaired"),
            "Hearing Impaired"
        );
        assert_eq!(en.get_localized_string("Default"), "Default");
        assert_eq!(en.get_localized_string("NoSuchPhrase"), "NoSuchPhrase");

        let de = LocalizationManager::default().with_ui_culture("de");
        assert_eq!(de.get_localized_string("Default"), "Standard");
        assert_eq!(de.get_localized_string("hearingimpaired"), "Hörgeschädigt");
        // An explicit culture overrides the server one; empty means the server's.
        assert_eq!(de.get_localized_string_for("Forced", "fr"), "Forcé");
        assert_eq!(de.get_localized_string_for("Forced", ""), "Erzwungen");
        // Region casing is normalized to the resource name.
        assert_eq!(
            de.get_localized_string_for("Undefined", "pt-br"),
            "Indefinido"
        );
        // BCP-47 `es-419` maps onto the underscore resource `es_419`.
        assert_eq!(
            de.get_localized_string_for("Default", "es-419"),
            "Por defecto"
        );
        // An unknown culture falls back to en-US.
        assert_eq!(de.get_localized_string_for("Default", "xx-YY"), "Default");
        // A phrase missing from the culture falls back to en-US, not the key:
        // the novelty `pr` (Pirate) catalog is sparse.
        assert_eq!(
            LocalizationManager::default()
                .with_ui_culture("ab")
                .get_localized_string("HearingImpaired"),
            "Hearing Impaired"
        );
        // Every embedded dictionary parses (the lazy table panics otherwise).
        assert!(CORE_DICTIONARY_DATA.len() > 100);
    }

    /// The manager as the HTTP layer actually holds it: an `Arc<dyn …>` behind the
    /// `ferrofin-traits` DI seam. The wire-order/wire-content assertions below go through
    /// this rather than the inherent methods, so the tested path is the shipped path —
    /// the trait impl's `get_cultures` in particular has a body of its own.
    fn shipped(metadata_country_code: &str) -> Arc<dyn LocalizationManagerTrait> {
        Arc::new(LocalizationManager::new(metadata_country_code))
    }

    #[test]
    fn find_language_by_various_codes() {
        let m = LocalizationManager::default();
        let by_two = m.find_language_info("en").expect("two-letter");
        assert_eq!(by_two.display_name, "English");
        let by_three_t = m.find_language_info("fra").expect("three-letter T");
        assert_eq!(by_three_t.two_letter_iso_language_name, "fr");
        let by_three_b = m.find_language_info("fre").expect("three-letter B");
        assert_eq!(by_three_b.two_letter_iso_language_name, "fr");
        let by_display = m.find_language_info("German").expect("display");
        // Name is the English display name (C# uses the 2-letter code only for region tags).
        assert_eq!(by_display.name, "German");
        assert!(m.find_language_info("zzz").is_none());
    }

    // The full iso6392.txt list is served (every row, ISO 639-1 code or not), each with
    // Name = the English display name — regression for the compact 21-language subset.
    #[test]
    fn cultures_cover_the_full_iso6392_list() {
        let m = LocalizationManager::new("US");
        let cultures = m.get_cultures();
        assert!(
            cultures.len() > 180,
            "expected the full culture list, got {}",
            cultures.len()
        );
        let abk = cultures
            .iter()
            .find(|c| c.two_letter_iso_language_name == "ab")
            .expect("Abkhazian present");
        assert_eq!(abk.name, "Abkhazian");
        assert_eq!(abk.display_name, "Abkhazian");
    }

    #[test]
    fn language_display_name_truncates_at_delimiter() {
        let m = LocalizationManager::default();
        assert_eq!(
            m.get_language_display_name("es").as_deref(),
            Some("Spanish")
        );
        assert_eq!(m.get_language_display_name("nl").as_deref(), Some("Dutch"));
        assert_eq!(m.get_language_display_name(""), None);
    }

    #[test]
    fn iso6392_b_to_t_maps_known_pairs() {
        let m = LocalizationManager::default();
        assert_eq!(m.try_get_iso6392_t_from_b("fre").as_deref(), Some("fra"));
        assert_eq!(m.try_get_iso6392_t_from_b("ger").as_deref(), Some("deu"));
        // "eng" has no distinct B code → no mapping.
        assert_eq!(m.try_get_iso6392_t_from_b("eng"), None);
    }

    #[test]
    fn countries_are_available() {
        let m = LocalizationManager::default();
        let countries = m.get_countries();
        assert!(
            countries
                .iter()
                .any(|c| c.two_letter_iso_region_name == "US")
        );
        assert!(countries.iter().any(|c| c.display_name == "United Kingdom"));
    }

    #[test]
    fn rating_score_resolves_numbers_and_names() {
        let m = LocalizationManager::default();
        assert_eq!(m.get_rating_score("PG-13", None).map(|s| s.score), Some(13));
        assert_eq!(m.get_rating_score("16+", None).map(|s| s.score), Some(16));
        assert_eq!(
            m.get_rating_score("TV-MA", Some("US")).map(|s| s.score),
            Some(17)
        );
        // Unrated tokens resolve to None.
        assert_eq!(m.get_rating_score("Unrated", None), None);
        // Multi-value: first resolvable wins.
        assert_eq!(
            m.get_rating_score("Unrated / PG", None).map(|s| s.score),
            Some(10)
        );
        // With all tables loaded, upstream's slash split resolves the `a`
        // fragment via BG's A=0. Only a whole `n/a` is an unrated marker.
        assert_eq!(
            m.get_rating_score("n/a / PG", None).map(|s| s.score),
            Some(0)
        );
        // Country-prefixed.
        assert_eq!(m.get_rating_score("US:R", None).map(|s| s.score), Some(17));
    }

    /// The `GetSingleRatingScore` front half (`LocalizationManager.cs:396-416`):
    /// unrated tokens are `None`, a bare integer (with or without `+`) is its
    /// own age, and the `Rated`/`Rated:`/`Rated :` spellings are removed case
    /// insensitively before the country tables are consulted. `Score`/`SubScore`
    /// are what `MigrateRatingLevels` writes into the inherited columns.
    #[rstest]
    #[case("12", Some((12, None)))]
    #[case("18+", Some((18, None)))]
    #[case("TV-MA", Some((17, Some(1))))]
    #[case("Rated R", Some((17, Some(0))))]
    #[case("rated: PG-13", Some((13, Some(0))))]
    #[case("Rated : R", Some((17, Some(0))))]
    #[case("unknown-junk", None)]
    #[case("N/A", None)]
    #[case("not rated", None)]
    #[case("NR", None)]
    #[case("SE:15 / SE:15+ / SE:Från 15 år", Some((15, None)))]
    #[case("R / unknown", Some((17, Some(0))))]
    // A country prefix the tables do not know recurses on the rating part
    // (`TryGetRatingScoreBySeparator`'s last branch); a known one resolves the
    // rating in that table, then as a bare number.
    #[case("US:PG-13", Some((13, Some(0))))]
    #[case("US-PG-13", Some((13, Some(0))))]
    #[case("Germany: 16", Some((16, None)))]
    fn single_rating_scores_follow_get_rating_score(
        #[case] rating: &str,
        #[case] expected: Option<(i32, Option<i32>)>,
    ) {
        let m = LocalizationManager::default();
        assert_eq!(
            m.get_rating_score(rating, None)
                .map(|s| (s.score, s.sub_score)),
            expected
        );
    }

    #[rstest]
    #[case("Rated R", "Rated ", "R")]
    #[case("rated R", "Rated ", "R")]
    #[case("RATED: PG", "Rated:", " PG")]
    #[case("PG", "Rated ", "PG")]
    #[case("Rated Rated X", "Rated ", "X")]
    #[case("", "Rated ", "")]
    fn remove_ignore_ascii_case_drops_every_occurrence(
        #[case] input: &str,
        #[case] needle: &str,
        #[case] expected: &str,
    ) {
        assert_eq!(remove_ignore_ascii_case(input, needle), expected);
    }

    #[test]
    fn parental_ratings_backfilled_and_sorted() {
        let m = LocalizationManager::default();
        let ratings = m.get_parental_ratings();
        // Back-filled entries present.
        assert!(ratings.iter().any(|r| r.name == "Unrated"));
        assert!(ratings.iter().any(|r| r.name == "XXX"));
        assert!(ratings.iter().any(|r| r.name == "Banned"));
        // Sorted ascending by score (ignoring the None-scored "Unrated").
        let scores: Vec<i32> = ratings
            .iter()
            .filter_map(|r| r.rating_score.map(|s| s.score))
            .collect();
        let mut sorted = scores.clone();
        sorted.sort_unstable();
        assert_eq!(scores, sorted);
    }

    #[test]
    fn vendored_localization_data_matches_jellyfin() {
        let m = LocalizationManager::default();
        // Countries: the full bundled ISO-3166 set (140, upstream `GetCountries_All_Success`),
        // incl. the region codes the old 10-row table lacked (029 = Caribbean, AT = Austria)
        // and Tanzania, added in Jellyfin 12.
        let countries = m.get_countries();
        assert_eq!(countries.len(), 140);
        assert!(countries.contains(&CountryInfo {
            name: "TZ".to_owned(),
            display_name: "Tanzania".to_owned(),
            two_letter_iso_region_name: "TZ".to_owned(),
            three_letter_iso_region_name: "TZA".to_owned(),
        }));
        assert!(countries.iter().any(|c| c.name == "029"));
        assert!(
            countries
                .iter()
                .any(|c| c.name == "AT" && c.display_name == "Austria")
        );

        // Parental ratings: NC-17 is score 17 / subScore 1 (grouped with TV-MA), not the old 18.
        let ratings = m.get_parental_ratings();
        let nc17 = ratings.iter().find(|r| r.name == "NC-17").expect("NC-17");
        assert_eq!(nc17.rating_score.map(|s| s.score), Some(17));
        assert_eq!(nc17.rating_score.and_then(|s| s.sub_score), Some(1));

        // Cultures: v12.2's iso6392.txt, where the Portuguese variants carry `por`
        // (upstream #15555 reverted the `pob`/`pop` codes).
        let pt_br = m
            .get_cultures()
            .iter()
            .find(|c| c.name == "pt-br")
            .expect("pt-br");
        assert_eq!(
            pt_br.three_letter_iso_language_names,
            vec!["por".to_owned()]
        );
        assert!(m.find_language_info("pob").is_none());
        assert!(m.find_language_info("pop").is_none());

        // Localization options cover every bundled translation, including es_419.
        let options = m.get_localization_options();
        assert_eq!(options.len(), 105);
        assert!(options.iter().any(|o| o.value == "es_419"));
        assert!(
            options
                .iter()
                .any(|o| o.value == "en-US" && o.name == "English")
        );
    }

    /// The `LazyLock` table must hand back the vendored file order, not a hashed or
    /// re-sorted one. `countries.json` is *roughly* `DisplayName`-ordered but not exactly
    /// (Pakistan sits between Ireland and Israel upstream), and the `Name` column is not
    /// alphabetical at all — either fact breaks under any re-sort.
    #[test]
    fn countries_preserve_vendored_file_order() {
        let m = shipped(DEFAULT_METADATA_COUNTRY_CODE);
        let countries = m.get_countries();
        assert_eq!(countries.len(), 140);

        let head: Vec<&str> = countries.iter().take(6).map(|c| c.name.as_str()).collect();
        assert_eq!(head, ["AF", "AL", "DZ", "AR", "AM", "AU"]);
        let tail: Vec<&str> = countries[137..].iter().map(|c| c.name.as_str()).collect();
        assert_eq!(tail, ["VN", "YE", "ZW"]);
        assert_eq!(
            countries[0],
            CountryInfo {
                name: "AF".to_owned(),
                display_name: "Afghanistan".to_owned(),
                two_letter_iso_region_name: "AF".to_owned(),
                three_letter_iso_region_name: "AFG".to_owned(),
            }
        );
        // The upstream file's one out-of-alphabetical row — an order fingerprint that any
        // re-sort (by name or by display name) would move.
        assert_eq!(countries[56].display_name, "Pakistan");
        assert_eq!(countries[57].display_name, "Israel");

        // Repeated calls are identical (the table is built once and cloned).
        assert_eq!(countries, m.get_countries());
        assert_eq!(countries, shipped("DE").get_countries());
    }

    /// The full `/Localization/ParentalRatings` payload, entry by entry, in order.
    /// This is the vendored `ratings-us.json` order (Jellyfin's dictionary insertion
    /// order) with `Unrated` sorting first and the 21/XXX/Banned back-fill last.
    #[test]
    fn parental_ratings_exact_contents_and_order() {
        /// One expected row: `(name, (score, sub-score))`.
        type Row<S> = (S, Option<(i32, Option<i32>)>);

        let m = shipped(DEFAULT_METADATA_COUNTRY_CODE);
        let expected: &[Row<&str>] = &[
            ("Unrated", None),
            ("Approved", Some((0, Some(0)))),
            ("G", Some((0, Some(0)))),
            ("TV-G", Some((0, Some(0)))),
            ("TV-Y", Some((0, Some(0)))),
            ("TV-Y7", Some((7, Some(0)))),
            ("TV-Y7-FV", Some((7, Some(1)))),
            ("PG", Some((10, Some(0)))),
            ("TV-PG", Some((10, Some(0)))),
            ("TV-PG-D", Some((10, Some(1)))),
            ("TV-PG-L", Some((10, Some(1)))),
            ("TV-PG-S", Some((10, Some(1)))),
            ("TV-PG-V", Some((10, Some(1)))),
            ("TV-PG-DL", Some((10, Some(1)))),
            ("TV-PG-DS", Some((10, Some(1)))),
            ("TV-PG-DV", Some((10, Some(1)))),
            ("TV-PG-LS", Some((10, Some(1)))),
            ("TV-PG-LV", Some((10, Some(1)))),
            ("TV-PG-SV", Some((10, Some(1)))),
            ("TV-PG-DLS", Some((10, Some(1)))),
            ("TV-PG-DLV", Some((10, Some(1)))),
            ("TV-PG-DSV", Some((10, Some(1)))),
            ("TV-PG-LSV", Some((10, Some(1)))),
            ("TV-PG-DLSV", Some((10, Some(1)))),
            ("PG-13", Some((13, Some(0)))),
            ("TV-14", Some((14, Some(0)))),
            ("TV-14-D", Some((14, Some(1)))),
            ("TV-14-L", Some((14, Some(1)))),
            ("TV-14-S", Some((14, Some(1)))),
            ("TV-14-V", Some((14, Some(1)))),
            ("TV-14-DL", Some((14, Some(1)))),
            ("TV-14-DS", Some((14, Some(1)))),
            ("TV-14-DV", Some((14, Some(1)))),
            ("TV-14-LS", Some((14, Some(1)))),
            ("TV-14-LV", Some((14, Some(1)))),
            ("TV-14-SV", Some((14, Some(1)))),
            ("TV-14-DLS", Some((14, Some(1)))),
            ("TV-14-DLV", Some((14, Some(1)))),
            ("TV-14-DSV", Some((14, Some(1)))),
            ("TV-14-LSV", Some((14, Some(1)))),
            ("TV-14-DLSV", Some((14, Some(1)))),
            ("R", Some((17, Some(0)))),
            ("NC-17", Some((17, Some(1)))),
            ("TV-MA", Some((17, Some(1)))),
            ("TV-MA-L", Some((17, Some(1)))),
            ("TV-MA-S", Some((17, Some(1)))),
            ("TV-MA-V", Some((17, Some(1)))),
            ("TV-MA-LS", Some((17, Some(1)))),
            ("TV-MA-LV", Some((17, Some(1)))),
            ("TV-MA-SV", Some((17, Some(1)))),
            ("TV-MA-LSV", Some((17, Some(1)))),
            ("TV-X", Some((18, Some(0)))),
            ("TV-AO", Some((18, Some(0)))),
            ("21", Some((21, None))),
            ("XXX", Some((1000, None))),
            ("Banned", Some((1001, None))),
        ];

        let actual: Vec<Row<String>> = m
            .get_parental_ratings()
            .into_iter()
            .map(|r| (r.name, r.rating_score.map(|s| (s.score, s.sub_score))))
            .collect();
        let expected: Vec<Row<String>> = expected
            .iter()
            .map(|(name, score)| ((*name).to_owned(), *score))
            .collect();
        assert_eq!(actual, expected);

        // `Value` mirrors `RatingScore.Score` for every entry (the deprecated field).
        for r in m.get_parental_ratings() {
            assert_eq!(r.value, r.rating_score.map(|s| s.score), "{}", r.name);
        }

        // Deterministic across calls *and* across manager instances — the point of the
        // ordered table (a HashMap-derived list would shuffle per process).
        assert_eq!(
            m.get_parental_ratings(),
            shipped("us").get_parental_ratings()
        );
    }

    /// The vendored rating data has no duplicate rating string, so the ordered list and
    /// the lookup index describe exactly the same set.
    #[test]
    fn rating_table_ordered_and_indexed_agree() {
        let table = parental_ratings_for("us").expect("US table");
        assert_eq!(table.ordered.len(), table.by_name.len());
        assert_eq!(table.ordered.len(), 52);
        for (name, score) in &table.ordered {
            assert_eq!(table.by_name.get(&rating_key(name)), Some(score));
        }
        assert!(parental_ratings_for("zz").is_none());
    }

    /// An unknown country exposes only the common backfill.
    #[test]
    fn parental_ratings_depend_on_the_configured_country() {
        let names: Vec<String> = shipped("ZZ")
            .get_parental_ratings()
            .into_iter()
            .map(|r| r.name)
            .collect();
        assert_eq!(
            names,
            [
                "Unrated", "Approved", "10", "13", "14", "21", "XXX", "Banned"
            ]
        );
    }

    /// No bundled language disappears from the dropdown; native labels and
    /// order match the pinned .NET catalog rather than the old web labels.
    #[test]
    fn localization_options_cover_resources_and_native_order() {
        let options = shipped(DEFAULT_METADATA_COUNTRY_CODE).get_localization_options();
        let expected: Vec<LocalizationOption> =
            serde_json::from_str(LOCALIZATION_OPTIONS_JSON).unwrap();
        assert_eq!(
            options, expected,
            "complete .NET catalog and label ordering"
        );
        let mut values: Vec<_> = options.iter().map(|o| o.value.as_str()).collect();
        values.sort_unstable();
        let mut resources: Vec<_> = core_dictionaries::CORE_DICTIONARIES
            .iter()
            .map(|(code, _)| *code)
            .collect();
        resources.sort_unstable();
        assert_eq!(values, resources, "exactly one option for each resource");
        for (code, name) in [
            ("lt-LT", "lietuvių (Lietuva)"),
            ("pr", "pr"),
            ("es_419", "español (Latinoamérica)"),
            ("ar_SA", "العربية (المملكة العربية السعودية)"),
        ] {
            assert_eq!(options.iter().find(|o| o.value == code).unwrap().name, name);
        }
        assert!(!values.contains(&"lt"));
        assert_eq!(options.first().unwrap().value, "ab");
        assert_eq!(options.last().unwrap().value, "ko");
    }

    /// Cultures come from the shared static but are still the full list, in file order.
    ///
    /// Driven through the trait seam: the trait impl's `get_cultures` has its own body
    /// (it clones the static, where the inherent one borrows it) and is what
    /// `GET /Localization/Cultures` calls.
    #[test]
    fn cultures_preserve_file_order() {
        let m = shipped(DEFAULT_METADATA_COUNTRY_CODE);
        let cultures = m.get_cultures();
        // Upstream `GetCultures_All_Success` (v12.2 `LocalizationManagerTests.cs:46`).
        assert_eq!(cultures.len(), 496);
        let head: Vec<(&str, &str)> = cultures
            .iter()
            .take(4)
            .map(|c| {
                (
                    c.three_letter_iso_language_name
                        .as_deref()
                        .unwrap_or_default(),
                    c.two_letter_iso_language_name.as_str(),
                )
            })
            .collect();
        // File order (ISO 639-2/T); a row with no ISO 639-1 code keeps an empty
        // two-letter code (Jellyfin 12 `LoadCultures`).
        assert_eq!(
            head,
            [("aar", "aa"), ("abk", "ab"), ("ace", ""), ("ach", "")]
        );
        assert_eq!(cultures[0].display_name, "Afar");
        assert_eq!(
            cultures.last().map(|c| c.display_name.as_str()),
            Some("Zaza; Dimili; Dimli; Kirdki; Kirmanjki; Zazaki")
        );
        // Region tags keep the two-letter column as their Name (C# behaviour).
        let hk = cultures
            .iter()
            .find(|c| c.two_letter_iso_language_name == "zh-hk")
            .expect("zh-hk present");
        assert_eq!(hk.name, "zh-hk");
        assert_eq!(hk.display_name, "Chinese (Hong Kong)");
        // The same static is handed out every time.
        assert_eq!(cultures, shipped("DE").get_cultures());
    }

    /// The trait impl is the only path the HTTP handlers take, so it must return exactly
    /// what the inherent methods do — the two `get_cultures` bodies especially.
    #[test]
    fn trait_impl_matches_the_inherent_methods() {
        let concrete = LocalizationManager::default();
        let via_trait: &dyn LocalizationManagerTrait = &concrete;

        assert_eq!(via_trait.get_cultures(), concrete.get_cultures());
        assert_eq!(via_trait.get_countries(), concrete.get_countries());
        assert_eq!(
            via_trait.get_parental_ratings(),
            concrete.get_parental_ratings()
        );
        assert_eq!(
            via_trait.get_localization_options(),
            concrete.get_localization_options()
        );
        for (rating, country) in [
            ("PG-13", None),
            ("TV-MA", Some("US")),
            ("16+", None),
            ("US:R", None),
            ("Unrated", None),
            ("nonsense", None),
        ] {
            assert_eq!(
                via_trait.get_rating_score(rating, country),
                concrete.get_rating_score(rating, country),
                "{rating}"
            );
        }
    }

    /// A repeated rating string must resolve to the *same* score on both wire surfaces:
    /// `GET /Localization/ParentalRatings` (built from `ordered`) and `get_rating_score`
    /// (which reads `by_name`). C#'s `dict[key] = value` overwrites in place, so the last
    /// score wins on both while the entry keeps its first-occurrence position.
    #[test]
    fn duplicate_rating_string_agrees_across_both_surfaces() {
        let json = r#"{
            "countryCode": "zz",
            "ratings": [
                { "ratingStrings": ["ZZ-A", "ZZ-DUP"], "ratingScore": { "score": 5 } },
                { "ratingStrings": ["ZZ-B"], "ratingScore": { "score": 12 } },
                { "ratingStrings": ["ZZ-DUP"], "ratingScore": { "score": 18, "subScore": 3 } }
            ]
        }"#;
        let table = load_rating_table(json);

        // One entry per distinct rating string, in first-occurrence order.
        let order: Vec<&str> = table.ordered.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(order, ["ZZ-A", "ZZ-DUP", "ZZ-B"]);
        assert_eq!(table.ordered.len(), table.by_name.len());

        // Last value wins, in both views.
        for (name, score) in &table.ordered {
            assert_eq!(table.by_name.get(&rating_key(name)), Some(score), "{name}");
        }
        let dup = table.by_name.get("ZZ-DUP").copied().expect("ZZ-DUP");
        assert_eq!((dup.score, dup.sub_score), (18, Some(3)));

        // And the two shipped surfaces therefore agree on the score for that rating:
        // the `/Localization/ParentalRatings` row vs. the `get_rating_score` lookup.
        let listed = build_parental_ratings_from(Some(&table))
            .into_iter()
            .find(|r| r.name == "ZZ-DUP")
            .expect("ZZ-DUP listed");
        assert_eq!(listed.rating_score, Some(dup));
        assert_eq!(listed.value, Some(18));
    }
    /// `GetRatingScore(value)` on a server whose `MetadataCountryCode` is
    /// `country` — the shape of every upstream rating `[Theory]`.
    fn score_on_server(rating: &str, country: &str) -> Option<(i32, Option<i32>)> {
        LocalizationManager::new(country)
            .get_rating_score(rating, None)
            .map(|s| (s.score, s.sub_score))
    }

    /// Upstream `GetRatingLevel_GivenValidString_Success` (v12.2
    /// `LocalizationManagerTests.cs:191-255`), every `[InlineData]`.
    #[rstest]
    #[case("CA-R", "CA", 18, Some(1))]
    #[case("FSK-16", "DE", 16, None)]
    #[case("FSK-18", "DE", 18, None)]
    #[case("FSK-18", "US", 18, None)]
    #[case("TV-MA", "US", 17, Some(1))]
    #[case("XXX", "asdf", 1000, None)]
    #[case("Germany: FSK-18", "DE", 18, None)]
    #[case("Rated : R", "US", 17, Some(0))]
    #[case("Rated: R", "US", 17, Some(0))]
    #[case("Rated R", "US", 17, Some(0))]
    #[case(" PG-13 ", "US", 13, Some(0))]
    #[case("T", "IT", 0, None)]
    #[case("VM6", "IT", 6, None)]
    #[case("VM12", "IT", 12, None)]
    #[case("VM14", "IT", 14, None)]
    #[case("VM18", "IT", 18, None)]
    #[case("IT-VM14", "IT", 14, None)] // TMDB style country prefix
    #[case("IT-VM18", "IT", 18, None)]
    #[case("it-vm18", "IT", 18, None)] // Rating strings are case insensitive
    #[case("VM 18", "IT", 18, None)]
    #[case("Vietato ai minori di 18 anni", "IT", 18, None)]
    #[case("ATP", "AR", 0, None)]
    #[case("SAM 13", "AR", 13, None)]
    #[case("SAM 16", "AR", 16, None)]
    #[case("SAM 18", "AR", 18, None)]
    #[case("SAM13", "AR", 13, None)] // Written without a space
    #[case("AR-SAM 16", "AR", 16, None)] // Country prefix stripped against the configured country
    #[case("AR-SAM 13", "US", 13, None)] // Country prefix resolved via the separator fallback
    #[case("AR-SAM 18", "US", 18, None)]
    #[case("AR-SAM13", "US", 13, None)]
    #[case("SAM 18 C", "AR", 1001, None)] // Condicionada, same as "C"
    #[case("Interdit aux moins de 12 ans", "FR", 12, None)]
    #[case("Interdit aux moins de 18 ans", "FR", 18, None)]
    #[case("X 18+", "AU", 1000, Some(0))] // Official spelling of the Australian X rating
    #[case("X18+", "AU", 1000, Some(0))]
    #[case("FSK18", "DE", 18, None)] // Written without a space
    #[case("ab 18", "DE", 18, None)] // Written as the minimum age
    #[case("DE:ab 6", "DE", 6, None)]
    #[case("\u{2013}12", "FR", 12, None)] // The CNC writes its minimum ages with an en dash
    #[case("\u{2013}16", "FR", 16, None)]
    #[case("\u{2013}18", "FR", 18, None)]
    #[case("SU", "ID", 0, None)] // Indonesian broadcast classifications (KPI)
    #[case("P", "ID", 2, None)]
    #[case("A", "ID", 7, None)]
    #[case("R", "ID", 13, None)]
    #[case("D", "ID", 18, None)]
    #[case("D18+", "ID", 18, None)] // Written with the minimum age, as broadcast since 2016
    #[case("R-BO", "ID", 13, None)] // Parental guidance does not change the age group
    #[case("Dewasa", "ID", 18, None)]
    #[case("ID-D", "ID", 18, None)] // TMDB style country prefix
    #[case("ID-D", "US", 18, None)] // Country prefix resolved via the separator fallback
    #[case("P", "US", 1000, None)] // Ambiguous outside Indonesia, preferred as the Portuguese "Pornográfico"
    fn get_rating_level_given_valid_string(
        #[case] rating: &str,
        #[case] country: &str,
        #[case] score: i32,
        #[case] sub_score: Option<i32>,
    ) {
        assert_eq!(score_on_server(rating, country), Some((score, sub_score)));
    }

    /// Upstream `GetRatingScore_IsCaseInsensitive_Success` (`:257-279`).
    #[rstest]
    #[case("btl", "se", 0, None)] // Direct lookup, lowercase of "Btl"
    #[case("BARNTILLÅTEN", "se", 0, None)] // Direct lookup, uppercase incl. diacritics
    #[case("SE-BTL", "se", 0, None)] // Country prefix stripped against the configured country
    #[case("SE-BTL", "us", 0, None)] // Country prefix resolved via the separator fallback
    #[case("Från 7 År", "se", 7, None)] // Diacritic casing (json has "Från 7 år")
    #[case("SE-Från 7 År", "us", 7, None)] // Same, via the separator fallback
    #[case("fsk-16", "de", 16, None)] // Not Sweden specific: lowercase of "FSK-16"
    fn get_rating_score_is_case_insensitive(
        #[case] rating: &str,
        #[case] country: &str,
        #[case] score: i32,
        #[case] sub_score: Option<i32>,
    ) {
        assert_eq!(score_on_server(rating, country), Some((score, sub_score)));
    }

    /// Upstream `GetRatingLevel_GivenValidAge_Success` (`:281-302`): a bare
    /// age is its own score, and the French CNC's leading `-` ("not for under
    /// 12s") is a minimum age — read as −12 it would sort below every
    /// `MaxParentalRatingScore` and bypass parental control entirely.
    #[rstest]
    #[case("0", 0)]
    #[case("1", 1)]
    #[case("6", 6)]
    #[case("12", 12)]
    #[case("42", 42)]
    #[case("9999", 9999)]
    #[case("-10", 10)]
    #[case("-12", 12)]
    #[case("-16", 16)]
    #[case("-18", 18)]
    fn get_rating_level_given_valid_age(#[case] rating: &str, #[case] score: i32) {
        assert_eq!(score_on_server(rating, "nl"), Some((score, None)));
    }

    /// `TryParseRatingAsScore` strips one or more leading `-` and trailing
    /// `+`, plus surrounding whitespace. The ` -12` and `12+ ` cases are
    /// Ferrofin's values, not a C# oracle: upstream reads ` -12` as −12 and
    /// fails `12+ ` (accepted divergence, see `parse_rating_as_score`).
    #[rstest]
    #[case("-12", Some(12))]
    #[case("--12", Some(12))]
    #[case("18+", Some(18))]
    #[case("-16+", Some(16))]
    #[case(" 12 ", Some(12))]
    #[case(" -12", Some(12))]
    #[case("- 12", Some(12))]
    #[case("12 +", Some(12))]
    #[case("12+ ", Some(12))]
    #[case("+12", Some(12))]
    #[case("-", None)]
    #[case("PG-13", None)]
    #[case("", None)]
    fn parse_rating_as_score_reads_minimum_ages(#[case] rating: &str, #[case] age: Option<i32>) {
        assert_eq!(parse_rating_as_score(rating), age);
    }

    /// Upstream `GetRatingLevel_GivenUnratedString_Success` (`:304-318`).
    #[rstest]
    #[case("NR")]
    #[case("unrated")]
    #[case("Not Rated")]
    #[case("n/a")]
    #[case("N/A")]
    #[case(" n/a ")]
    fn get_rating_level_given_unrated_string(#[case] rating: &str) {
        assert_eq!(score_on_server(rating, DEFAULT_METADATA_COUNTRY_CODE), None);
    }

    /// Upstream `GetRatingLevel_SkipsUnratedListEntries_Success` (`:320-335`):
    /// `NR`/`UR` are rating strings of some systems, so they must stay unrated
    /// when listed alongside others.
    #[rstest]
    #[case("NR / R", 17, Some(0))]
    #[case("unrated / R", 17, Some(0))]
    #[case("R / NR", 17, Some(0))]
    fn get_rating_level_skips_unrated_list_entries(
        #[case] rating: &str,
        #[case] score: i32,
        #[case] sub_score: Option<i32>,
    ) {
        assert_eq!(score_on_server(rating, "us"), Some((score, sub_score)));
    }

    /// Upstream `GetRatingScore_RatingContainingSlash_IsNotSplit` (`:337-361`).
    #[rstest]
    #[case("M/3", "pt", 3, None)]
    #[case("M/12", "pt", 12, None)]
    #[case("M/18", "pt", 18, None)]
    #[case("PT-M/12", "pt", 12, None)] // TMDB style country prefix
    #[case("M/12", "us", 12, None)] // Resolved through the all-systems fallback
    #[case("U/A 13+", "in", 13, None)]
    #[case("7/i", "es", 11, None)]
    #[case("7/i/fig", "es", 11, None)]
    #[case("18/fig", "es", 18, None)]
    fn get_rating_score_rating_containing_slash_is_not_split(
        #[case] rating: &str,
        #[case] country: &str,
        #[case] score: i32,
        #[case] sub_score: Option<i32>,
    ) {
        assert_eq!(score_on_server(rating, country), Some((score, sub_score)));
    }

    /// Upstream `GetRatingScore_CountryPrefixedList_UsesFirstResolvingEntry`
    /// (`:363-379`): a list of country-prefixed spellings is split by `/`
    /// first — the separator split of the whole value would pair the first
    /// entry's country with the last entry's rating.
    #[rstest]
    #[case("DE:FSK 18 / DE:FSK-18 / DE:FSK18 / DE:18 / DE:ab 18", 18, None)]
    #[case("SE:15 / SE:15+ / SE:Från 15 år", 15, None)]
    #[case("FR:16 / US:12", 16, None)] // The first entry that resolves wins
    fn get_rating_score_country_prefixed_list_uses_first_resolving_entry(
        #[case] rating: &str,
        #[case] score: i32,
        #[case] sub_score: Option<i32>,
    ) {
        assert_eq!(score_on_server(rating, "de"), Some((score, sub_score)));
    }

    /// Counts this module's log events: those at `DEBUG`, and those above it.
    #[derive(Clone, Default)]
    struct CountRatingLogs {
        /// Events at `DEBUG`.
        debug: Arc<std::sync::atomic::AtomicUsize>,
        /// Events at `INFO`, `WARN` or `ERROR`.
        above_debug: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for CountRatingLogs {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let metadata = event.metadata();
            if metadata.target() != "ferrofin_core::localization_manager" {
                return;
            }
            let counter = match *metadata.level() {
                tracing::Level::DEBUG => &self.debug,
                tracing::Level::TRACE => return,
                _ => &self.above_debug,
            };
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// Resolves `rating` on a `de` server under a counting subscriber,
    /// returning the score and the `(debug, above debug)` event counts.
    fn score_with_log_counts(rating: &str) -> (Option<ParentalRatingScore>, (usize, usize)) {
        use tracing_subscriber::layer::SubscriberExt as _;
        let counts = CountRatingLogs::default();
        let score = tracing::subscriber::with_default(
            tracing_subscriber::registry().with(counts.clone()),
            || LocalizationManager::new("de").get_rating_score(rating, None),
        );
        let load = |n: &std::sync::atomic::AtomicUsize| n.load(std::sync::atomic::Ordering::SeqCst);
        (score, (load(&counts.debug), load(&counts.above_debug)))
    }

    /// Upstream `GetRatingScore_ResolvedCountryPrefixedList_DoesNotWarn`
    /// (`:381-398`): an entry of a list that resolves leaves no message, even
    /// though the whole value went through the lookups first.
    #[test]
    fn get_rating_score_resolved_country_prefixed_list_does_not_log() {
        let (score, counts) =
            score_with_log_counts("DE:FSK 18 / DE:FSK-18 / DE:FSK18 / DE:18 / DE:ab 18");
        assert!(score.is_some());
        assert_eq!(counts, (0, 0));
    }

    /// Upstream `GetRatingScore_ListWithoutKnownRating_WarnsOnce`
    /// (`:400-417`): a list none of whose entries resolves logs once for the
    /// whole list, not once per entry — at `debug` here (upstream: warning),
    /// because it fires per scanned item.
    #[test]
    fn get_rating_score_list_without_known_rating_logs_once_at_debug() {
        let (score, counts) = score_with_log_counts("DE:Unbekannt / DE:Unsinn");
        assert_eq!(score, None);
        assert_eq!(counts, (1, 0), "one debug event, nothing above debug");
        // A single unknown country-prefixed value is reported by the separator
        // lookup (once), and an explicitly unrated one not at all.
        assert_eq!(score_with_log_counts("DE:Unbekannt"), (None, (1, 0)));
        assert_eq!(score_with_log_counts("IT-NR"), (None, (0, 0)));
    }

    /// Upstream `GetRatingLevel_Split_Success` (`:419-431`).
    #[rstest]
    #[case("-NO RATING SHOWN-")]
    #[case(":NO RATING SHOWN:")]
    fn get_rating_level_split(#[case] rating: &str) {
        assert_eq!(score_on_server(rating, DEFAULT_METADATA_COUNTRY_CODE), None);
    }

    /// Upstream `GetRatingScore_FallbackPrioritizesUS_Success` (`:433-450`)
    /// and `GetRatingScore_ValidRatingWithCountrySeparator_ReturnsScore`
    /// (`:468-484`).
    #[rstest]
    #[case("TV-MA", "DE", 17, Some(1))] // US-only rating, DE country code
    #[case("PG-13", "FR", 13, Some(0))] // US-only rating, FR country code
    #[case("R", "JP", 17, Some(0))] // US-only rating, JP country code
    #[case("us:R", "DE", 17, Some(0))] // Colon separator, explicit US country, valid US rating
    #[case("US:PG-13", "DE", 13, Some(0))] // Colon separator, explicit US country, valid US rating
    #[case("ca:R", "US", 18, Some(1))] // Colon separator, Canada country code, valid CA rating
    fn get_rating_score_us_fallback_and_country_separator(
        #[case] rating: &str,
        #[case] country: &str,
        #[case] score: i32,
        #[case] sub_score: Option<i32>,
    ) {
        assert_eq!(score_on_server(rating, country), Some((score, sub_score)));
    }

    /// Upstream `GetRatingScore_UnknownRatingWithKnownCountry_ReturnsNull`
    /// (`:452-466`).
    #[rstest]
    #[case("US:INVALID")] // Colon separator, known country code, unknown rating
    #[case("us:INVALID")] // Colon separator, lowercase country code
    #[case("DE-INVALID")] // Hyphen separator, known language prefix, unknown rating
    #[case("ca:INVALID")] // Colon separator, known country code (Canada)
    fn get_rating_score_unknown_rating_with_known_country(#[case] rating: &str) {
        assert_eq!(score_on_server(rating, "US"), None);
    }

    /// D4 (owner decision, accepted divergence): Ferrofin splits a
    /// country-prefixed rating at the FIRST separator on both sides; upstream
    /// reads the rating after the LAST one. These values are Ferrofin's, not
    /// Jellyfin 12.2's.
    #[rstest]
    // D4: upstream null — it looks up `MA` in the Canadian table.
    #[case("CA-TV-MA", Some((18, Some(1))))]
    // D4: upstream 13/none — it parses `13` as a bare number.
    #[case("US-PG-13", Some((13, Some(0))))]
    fn d4_country_prefix_splits_at_the_first_separator(
        #[case] rating: &str,
        #[case] expected: Option<(i32, Option<i32>)>,
    ) {
        assert_eq!(score_on_server(rating, "ZZ"), expected);
    }

    /// Accepted divergence (a 12.1 regression, still in 12.2; rc7 and Ferrofin
    /// before both rated these): a `/`-list whose only non-empty part is not
    /// the whole value looks that part up instead of answering unrated, which
    /// would loosen parental filtering. Upstream 12.2 returns null for all of
    /// them. A surviving bare unrated marker stays unrated (`NR` would otherwise
    /// hit Slovakia's `NR` = 0 and turn an unrated item all-ages).
    #[rstest]
    #[case("PG/", Some((10, Some(0))))]
    #[case(" / PG", Some((10, Some(0))))]
    #[case("PG / ", Some((10, Some(0))))]
    #[case("US:PG/", Some((10, Some(0))))]
    #[case("NR/", None)]
    #[case(" / NR", None)]
    #[case("unrated / ", None)]
    fn a_list_with_one_surviving_part_looks_that_part_up(
        #[case] rating: &str,
        #[case] expected: Option<(i32, Option<i32>)>,
    ) {
        assert_eq!(score_on_server(rating, "US"), expected);
    }

    /// Upstream `GetParentalRatings_Default_Success` and
    /// `GetParentalRatings_ConfiguredCountryCode_Success` (`:157-189`).
    #[rstest]
    #[case("US", 56, "TV-MA", 17)]
    #[case("DE", 34, "FSK-12", 12)]
    fn get_parental_ratings_for_the_configured_country(
        #[case] country: &str,
        #[case] count: usize,
        #[case] name: &str,
        #[case] score: i32,
    ) {
        let ratings = shipped(country).get_parental_ratings();
        assert_eq!(ratings.len(), count);
        let rating = ratings
            .iter()
            .find(|r| r.name == name)
            .expect("rating listed");
        assert_eq!(rating.rating_score.map(|s| s.score), Some(score));
    }

    /// The vendored rating resources are Jellyfin v12.2's (`ca0f16eb7c`): 45
    /// files, 44 systems (`gb.json` and `uk.json` both declare `GB`), none
    /// empty, `0-PREFER` first in the fallback walk. The files are
    /// byte-compared against `v12.2` when re-vendored; the entries below exist
    /// only in the 12.2 (or 12.1) versions of their files.
    #[test]
    fn every_vendored_rating_table_matches_jellyfin_12_2() {
        assert_eq!(parental_ratings::TABLES.len(), 45);
        assert_eq!(PARENTAL_RATINGS.len(), 44);
        assert_eq!(
            PARENTAL_RATINGS.keys().next().map(String::as_str),
            Some("0-PREFER")
        );
        for (country, json) in parental_ratings::TABLES {
            let table = load_rating_table(json);
            assert!(!table.ordered.is_empty(), "{country} parses with ratings");
            assert_eq!(
                PARENTAL_RATINGS[*country].by_name.len(),
                table.by_name.len(),
                "{country}"
            );
        }
        for (country, rating, score) in [
            ("AR", "SAM 13", 13),
            ("AU", "X 18+", 1000),
            ("DE", "ab 18", 18),
            ("FR", "\u{2013}12", 12),
            ("ID", "D18+", 18),
            ("IT", "VM14", 14),
        ] {
            assert_eq!(
                parental_ratings_for(country)
                    .and_then(|t| t.by_name.get(&rating_key(rating)))
                    .map(|s| s.score),
                Some(score),
                "{country} {rating}"
            );
        }
    }

    /// The vendored `Core/*.json` dictionaries are Jellyfin v12.2's
    /// (`ca0f16eb7c`): 105 cultures, byte-compared against `v12.2` when
    /// re-vendored. Each phrase below has this value only in the 12.x version
    /// of its culture's file and differs from en-US, one per culture whose
    /// phrases changed in the v12.2 re-vendor (en-US only reordered keys), so a
    /// stale copy fails here instead of drifting silently.
    #[test]
    fn every_vendored_core_dictionary_matches_jellyfin_12_2() {
        assert_eq!(core_dictionaries::CORE_DICTIONARIES.len(), 105);
        let manager = LocalizationManager::default().with_ui_culture("en-US");
        for (culture, phrase, expected) in [
            (
                "ar",
                "TaskDownloadMissingLyricsDescription",
                "تحميل كلمات الأغاني",
            ),
            (
                "be",
                "LyricDownloadFailureFromForItem",
                "Не ўдалося загрузіць тэкст песні з {0} для {1}",
            ),
            ("bg-BG", "Original", "Оригинал"),
            ("da", "TvShows", "Serier"),
            ("de", "Artists", "Künstler"),
            ("en-GB", "NameSeasonNumber", "Series {0}"),
            ("eo", "Original", "Originala"),
            (
                "es-MX",
                "LyricDownloadFailureFromForItem",
                "No se pudo descargar la letra desde {0} para {1}",
            ),
            ("fo", "NameSeasonNumber", "Umfar {0}"),
            (
                "gl",
                "LyricDownloadFailureFromForItem",
                "Non se puideron descargar as letras desde {0} para {1}",
            ),
            ("hi", "Original", "असली"),
            ("hy", "Default", "Լռելյայն"),
            (
                "is",
                "TaskRefreshChannelsDescription",
                "Endurhleður upplýsingum netrása.",
            ),
            (
                "lb",
                "LyricDownloadFailureFromForItem",
                "Feeler beim Download vun de Songtexter vun {0} fir {1}",
            ),
            ("lt-LT", "ChapterNameValue", "Skyrius{0}"),
            (
                "ms",
                "LyricDownloadFailureFromForItem",
                "Lirik gagal dimuat turun dari {0} untuk {1}",
            ),
            ("oc", "HearingImpaired", "Amb de deficiéncias auditivas"),
            ("pt-PT", "NameSeasonUnknown", "Temporada desconhecida"),
            ("pt", "NameSeasonUnknown", "Temporada desconhecida"),
            (
                "sk",
                "TaskKeyframeExtractorDescription",
                "Extrahuje kľúčové snímky z videosúborov na vytvorenie presnejších HLS zoznamov. Táto úloha môže trvať dlhší čas.",
            ),
            ("ta", "Original", "அசல்"),
            ("th", "Original", "ต้นฉบับ"),
            ("tr", "AppDeviceValues", "Uygulama: {0}, Cihaz: {1}"),
            (
                "vi",
                "FailedLoginAttemptWithUserName",
                "Cố gắng đăng nhập thất bại từ {0}",
            ),
            ("zh-HK", "HeaderFavoriteEpisodes", "心水劇集"),
        ] {
            assert_eq!(
                manager.get_localized_string_for(phrase, culture),
                expected,
                "{culture} {phrase}"
            );
        }
    }

    /// Upstream `GetLocalizedString_*` (`:486-566`): en-US keys, an unknown
    /// key, a culture's own translation, an unknown culture's en-US fallback,
    /// and BCP-47 spellings of underscore resources (`es-419`, `he-IL`).
    #[rstest]
    #[case("Default", "", "Default")]
    #[case("HeaderLiveTV", "", "Live TV")]
    #[case(
        "SuperInvalidTranslationKeyThatWillNeverBeAdded",
        "",
        "SuperInvalidTranslationKeyThatWillNeverBeAdded"
    )]
    #[case("Artists", "de", "Künstler")]
    #[case("Artists", "zz", "Artists")]
    #[case("Books", "he-IL", "ספרים")]
    fn get_localized_string_cases(
        #[case] phrase: &str,
        #[case] culture: &str,
        #[case] expected: &str,
    ) {
        let manager = LocalizationManager::default().with_ui_culture("en-US");
        assert_eq!(manager.get_localized_string_for(phrase, culture), expected);
        assert_ne!(
            manager.get_localized_string_for("Default", "es-419"),
            "Default"
        );
    }

    /// Upstream `FindLanguageInfo_Valid_Success` (`:81-102`),
    /// `TryGetISO6392TFromB_Success` (`:57-79`) and the language display-name
    /// theories (`:123-155`).
    #[rstest]
    #[case("de")]
    #[case("deu")]
    #[case("ger")]
    #[case("german")]
    fn find_language_info_valid(#[case] identifier: &str) {
        let m = LocalizationManager::default();
        let german = m.find_language_info(identifier).expect("German");
        assert_eq!(
            german.three_letter_iso_language_name.as_deref(),
            Some("deu")
        );
        assert_eq!(german.display_name, "German");
        assert_eq!(german.name, "German");
        assert!(
            german
                .three_letter_iso_language_names
                .iter()
                .any(|n| n == "deu")
        );
        assert!(
            german
                .three_letter_iso_language_names
                .iter()
                .any(|n| n == "ger")
        );
        assert_eq!(m.try_get_iso6392_t_from_b("ger").as_deref(), Some("deu"));
        assert_eq!(m.try_get_iso6392_t_from_b("chi").as_deref(), Some("zho"));
        assert_eq!(m.try_get_iso6392_t_from_b("eng"), None);
    }

    /// Upstream `FindLanguageInfo_ISO6392Only_Success` (v12.2
    /// `LocalizationManagerTests.cs:104-121`): ISO 639-2-only codes resolve,
    /// with the raw, untruncated English name.
    #[rstest]
    #[case("mul", "Multiple languages")]
    #[case("und", "Undetermined")]
    #[case("mis", "Uncoded languages")]
    #[case("zxx", "No linguistic content; Not applicable")]
    fn find_language_info_iso6392_only(#[case] code: &str, #[case] expected_display_name: &str) {
        let m = LocalizationManager::default();
        let culture = m.find_language_info(code).expect("ISO 639-2-only culture");
        assert_eq!(culture.display_name, expected_display_name);
        assert_eq!(
            culture.three_letter_iso_language_name.as_deref(),
            Some(code)
        );
        assert_eq!(culture.two_letter_iso_language_name, "");
    }

    /// The trait seam's `find_language_info` is the inherent lookup, cloned.
    #[test]
    fn trait_find_language_info_matches_the_inherent_lookup() {
        let m = LocalizationManager::default();
        let via_trait: &dyn LocalizationManagerTrait = &m;
        let german = via_trait.find_language_info("ger").expect("German");
        assert_eq!(german.display_name, "German");
        assert_eq!(Some(&german), m.find_language_info("ger"));
        assert!(via_trait.find_language_info("").is_none());
        assert!(via_trait.find_language_info("zzz").is_none());
    }

    /// The indexed lookup is the C# `FirstOrDefault` scan: for every key of
    /// every culture (and tokens that miss, or that several cultures share, as
    /// `chi`/`zho` do across the seven Chinese rows) it returns the same
    /// culture the linear four-way predicate finds first in file order.
    #[test]
    fn indexed_find_language_info_equals_the_linear_scan() {
        let linear = |token: &str| {
            if token.is_empty() {
                return None;
            }
            CULTURE_DATA.cultures.iter().find(|c| {
                token.eq_ignore_ascii_case(&c.display_name)
                    || token.eq_ignore_ascii_case(&c.name)
                    || token.eq_ignore_ascii_case(&c.two_letter_iso_language_name)
                    || c.three_letter_iso_language_names
                        .iter()
                        .any(|n| token.eq_ignore_ascii_case(n))
            })
        };
        let mut tokens: Vec<String> = [
            "und", "xx", "cn", "zh-CN", "Chinese", "CHI", "N/A", "", " en",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        for culture in &CULTURE_DATA.cultures {
            tokens.push(culture.display_name.clone());
            tokens.push(culture.name.to_ascii_uppercase());
            tokens.push(culture.two_letter_iso_language_name.clone());
            tokens.extend(culture.three_letter_iso_language_names.iter().cloned());
        }
        for token in &tokens {
            assert_eq!(
                find_culture(token).map(std::ptr::from_ref),
                linear(token).map(std::ptr::from_ref),
                "token {token:?}"
            );
        }
    }

    /// The `BaseItem.OriginalLanguage` setter: a known token becomes its
    /// two-letter code, an unknown one (TMDB's `xx`/`cn`, OMDb's `N/A`) is
    /// kept, and an ISO 639-2-only culture's empty code is NOT fallen back
    /// from (`?? value` only catches null).
    #[rstest]
    #[case("English", "en")]
    #[case("eng", "en")]
    #[case("en", "en")]
    #[case("EN", "en")]
    #[case("ger", "de")]
    #[case("German", "de")]
    #[case("fra", "fr")]
    #[case("Chinese", "zh")]
    #[case("zh-CN", "zh-cn")]
    #[case("ja", "ja")]
    #[case("xx", "xx")]
    #[case("cn", "cn")]
    #[case("N/A", "N/A")]
    #[case(" ja ", " ja ")]
    #[case("und", "")]
    #[case("", "")]
    // v12.2's iso6392.txt gives the Portuguese variant rows `por`, not `pob`/`pop`
    // (upstream #15555), so those tokens are unknown and kept.
    #[case("por", "pt")]
    #[case("pt-br", "pt-br")]
    #[case("pob", "pob")]
    #[case("pop", "pop")]
    fn normalize_original_language_cases(#[case] value: &str, #[case] expected: &str) {
        assert_eq!(normalize_original_language(value), expected);
    }

    /// The trait seam's `try_get_iso6392_t_from_b` is the inherent lookup
    /// (`LocalizationManagerTests.TryGetISO6392TFromB_Success`).
    #[rstest]
    #[case("ger", Some("deu"))]
    #[case("fre", Some("fra"))]
    #[case("chi", Some("zho"))]
    #[case("eng", None)]
    // Case-insensitive, as C#'s `StringComparer.OrdinalIgnoreCase` dictionary.
    #[case("GER", Some("deu"))]
    #[case("Fre", Some("fra"))]
    #[case("ENG", None)]
    fn trait_try_get_iso6392_t_from_b(#[case] iso_b: &str, #[case] expected: Option<&str>) {
        let m = LocalizationManager::default();
        let via_trait: &dyn LocalizationManagerTrait = &m;
        assert_eq!(
            via_trait.try_get_iso6392_t_from_b(iso_b).as_deref(),
            expected
        );
    }

    #[rstest]
    #[case("ell", Some("Greek"))] // Comma truncation
    #[case("nld", Some("Dutch"))] // Semicolon truncation
    #[case("ron", Some("Romanian"))] // Semicolon truncation, multiple
    #[case("eng", Some("English"))] // No truncation
    #[case("zh-CN", Some("Chinese (Simplified)"))] // No truncation, with parentheses
    #[case("", None)]
    #[case("xyz", None)]
    fn get_language_display_name_cases(#[case] language: &str, #[case] expected: Option<&str>) {
        assert_eq!(
            LocalizationManager::default()
                .get_language_display_name(language)
                .as_deref(),
            expected
        );
    }

    #[test]
    fn live_country_changes_rating_choices_and_ambiguous_scores() {
        let country = Arc::new(std::sync::RwLock::new("US".to_owned()));
        let read = Arc::clone(&country);
        let manager = LocalizationManager::default()
            .with_metadata_country_source(move || read.read().unwrap().clone());
        assert_eq!(manager.get_parental_ratings().len(), 56);
        assert_eq!(manager.get_rating_score("PG", None).unwrap().score, 10);
        *country.write().unwrap() = "DE".into();
        let ratings = manager.get_parental_ratings();
        assert_eq!(ratings.len(), 34);
        assert_eq!(
            ratings.iter().find(|r| r.name == "FSK-12").unwrap().value,
            Some(12)
        );
        *country.write().unwrap() = "CA".into();
        assert_eq!(manager.get_rating_score("PG", None).unwrap().score, 8);
        assert_eq!(
            manager.get_rating_score("PG", Some("US")).unwrap().score,
            10
        );
        // Both gb.json and uk.json declare GB, as in upstream LoadAll.
        assert_eq!(parental_ratings::TABLES.len(), 45);
        assert_eq!(PARENTAL_RATINGS.len(), 44);
        // Cross-country fallback order must prefer the 0-prefer resource.
        assert_eq!(
            LocalizationManager::new("ZZ")
                .get_rating_score("M", None)
                .unwrap()
                .score,
            18
        );
    }

    #[test]
    fn startup_supported_ui_cultures_follow_all_embedded_resources() {
        let manager = LocalizationManager::default();
        let names =
            ferrofin_traits::localization::LocalizationManager::get_supported_ui_cultures(&manager);
        assert_eq!(names.len(), 105, "pinned embedded resource set");
        for name in [
            "en-US", "ar-SA", "lt-LT", "he-IL", "ur", "sv", "uz", "ka", "km",
        ] {
            assert!(
                names.iter().any(|actual| actual == name),
                "missing startup culture {name}"
            );
        }
        assert!(!names.iter().any(|name| name.contains('_')));
    }
}
