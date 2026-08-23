//! [`LocalizationManager`] — a concrete culture/country/parental-rating service.
//!
//! Port of `Emby.Server.Implementations.Localization.LocalizationManager`, cut
//! down to the culture-data surface that the rest of Ferrofin needs today.
//!
//! **Flagged for `ferrofin-traits`:** the C# `ILocalizationManager` interface was
//! deliberately *not* ported into `ferrofin-traits` (its module doc says the
//! service is "not part of the wire contract, so it is dropped"). Consumers of
//! this data (the DTO layer's language display names, the metadata provider's
//! rating scores) will need a trait to inject it. This unit ships a **concrete
//! struct**; a follow-up should add an `ILocalizationManager` trait to
//! `ferrofin-traits` and have this type implement it. The public methods here are
//! named to match that future trait so the change is mechanical.
//!
//! Dataset scope (a *minimal* embedded dataset, not the full Jellyfin resource
//! bundle):
//! - **Cultures** — a compact ISO 639 table (`(t, b, two-letter, display)`)
//!   covering the common media languages, parsed the same way C# parses
//!   `iso6392.txt` (building the ISO 639-2/B → /T map as a side effect).
//! - **Countries** — a short [`CountryInfo`] list for the common metadata
//!   regions.
//! - **Parental ratings** — the US rating system (the primary source of ratings
//!   in metadata providers), with the same "common ratings" back-fill and score
//!   ordering the C# `GetParentalRatings` performs.
//!
//! The localized-*string* machinery (`GetLocalizedString`, the per-culture JSON
//! resource dictionaries) is out of scope for this minimal port and is omitted;
//! the runtime translation catalog is a `ferrofin-server` asset concern.
//!
//! All the vendored tables (cultures, countries, ratings, UI-language options) are
//! parsed **once** into process-wide [`LazyLock`] statics rather than being rebuilt on
//! every call: the source data is `include_str!`-embedded and immutable, so nothing
//! about it can vary at runtime. The only per-instance state left is the configured
//! default country code and the parental-rating list derived from it.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use ferrofin_model::entities_media::{ParentalRating, ParentalRatingScore};
use ferrofin_model::globalization::{CountryInfo, CultureDto, LocalizationOption};

/// The server default culture, used when no country/culture is supplied.
const DEFAULT_METADATA_COUNTRY_CODE: &str = "US";

/// Values that mean "no rating" and resolve to `None` (C# `_unratedValues`).
const UNRATED_VALUES: &[&str] = &["n/a", "unrated", "not rated", "nr"];

/// Jellyfin's embedded ISO 639-2 language table (`iso6392.txt`), one language per line,
/// pipe-delimited `iso639-2/T | iso639-2/B | iso639-1 | English name | French name`. Ported
/// verbatim from upstream so `GET /Localization/Cultures` yields the same ~200-language list.
const ISO6392: &str = include_str!("data/iso6392.txt");

/// Jellyfin's bundled country dataset, vendored verbatim from
/// `Emby.Server.Implementations/Localization/countries.json` (v10.11.8, 139 ISO-3166 entries).
const COUNTRIES_JSON: &str = include_str!("data/countries.json");

/// Jellyfin's bundled US parental-rating system, vendored verbatim from
/// `Emby.Server.Implementations/Localization/Ratings/us.json` (v10.11.8).
const RATINGS_US_JSON: &str = include_str!("data/ratings-us.json");

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

/// The fixed UI-language option list, ported verbatim from Jellyfin's
/// `LocalizationManager.GetLocalizationOptions` (v10.11.8). `(name, value)`.
const LOCALIZATION_OPTIONS: &[(&str, &str)] = &[
    ("Afrikaans", "af"),
    ("العربية", "ar"),
    ("Беларуская", "be"),
    ("Български", "bg-BG"),
    ("বাংলা (বাংলাদেশ)", "bn"),
    ("Català", "ca"),
    ("Čeština", "cs"),
    ("Cymraeg", "cy"),
    ("Dansk", "da"),
    ("Deutsch", "de"),
    ("English (United Kingdom)", "en-GB"),
    ("English", "en-US"),
    ("Ελληνικά", "el"),
    ("Esperanto", "eo"),
    ("Español", "es"),
    ("Español americano", "es_419"),
    ("Español (Argentina)", "es-AR"),
    ("Español (Dominicana)", "es_DO"),
    ("Español (México)", "es-MX"),
    ("Eesti", "et"),
    ("Basque", "eu"),
    ("فارسی", "fa"),
    ("Suomi", "fi"),
    ("Filipino", "fil"),
    ("Français", "fr"),
    ("Français (Canada)", "fr-CA"),
    ("Galego", "gl"),
    ("Schwiizerdütsch", "gsw"),
    ("עִבְרִית", "he"),
    ("हिन्दी", "hi"),
    ("Hrvatski", "hr"),
    ("Magyar", "hu"),
    ("Bahasa Indonesia", "id"),
    ("Íslenska", "is"),
    ("Italiano", "it"),
    ("日本語", "ja"),
    ("Qazaqşa", "kk"),
    ("한국어", "ko"),
    ("Lietuvių", "lt"),
    ("Latviešu", "lv"),
    ("Македонски", "mk"),
    ("മലയാളം", "ml"),
    ("मराठी", "mr"),
    ("Bahasa Melayu", "ms"),
    ("Norsk bokmål", "nb"),
    ("नेपाली", "ne"),
    ("Nederlands", "nl"),
    ("Norsk nynorsk", "nn"),
    ("ਪੰਜਾਬੀ", "pa"),
    ("Polski", "pl"),
    ("Pirate", "pr"),
    ("Português", "pt"),
    ("Português (Brasil)", "pt-BR"),
    ("Português (Portugal)", "pt-PT"),
    ("Românește", "ro"),
    ("Русский", "ru"),
    ("Slovenčina", "sk"),
    ("Slovenščina", "sl-SI"),
    ("Shqip", "sq"),
    ("Српски", "sr"),
    ("Svenska", "sv"),
    ("தமிழ்", "ta"),
    ("తెలుగు", "te"),
    ("ภาษาไทย", "th"),
    ("Türkçe", "tr"),
    ("Українська", "uk"),
    ("اُردُو", "ur_PK"),
    ("Tiếng Việt", "vi"),
    ("汉语 (简体字)", "zh-CN"),
    ("漢語 (繁體字)", "zh-TW"),
    ("廣東話 (香港)", "zh-HK"),
];

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
    // Port of C# GetCountries: deserialize the bundled countries.json (139 ISO-3166 regions).
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

/// The UI-language option list, materialized once from [`LOCALIZATION_OPTIONS`].
static LOCALIZATION_OPTION_LIST: LazyLock<Vec<LocalizationOption>> = LazyLock::new(|| {
    LOCALIZATION_OPTIONS
        .iter()
        .map(|(name, value)| LocalizationOption {
            name: (*name).to_owned(),
            value: (*value).to_owned(),
        })
        .collect()
});

/// The parsed ISO 639-2 dataset, built once.
static CULTURE_DATA: LazyLock<CultureData> = LazyLock::new(load_culture_data);

/// Every known rating system, keyed by upper-case country code, parsed once.
static PARENTAL_RATINGS: LazyLock<HashMap<String, RatingTable>> = LazyLock::new(|| {
    let mut systems = HashMap::new();
    systems.insert("US".to_owned(), load_rating_table(RATINGS_US_JSON));
    systems
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

/// A concrete culture/country/parental-rating service over a minimal dataset.
///
/// See the module docs: this stands in for the (unported) C#
/// `ILocalizationManager` and should be fronted by a `ferrofin-traits` trait in a
/// follow-up.
#[derive(Clone)]
pub struct LocalizationManager {
    /// The server default country code for rating fallback.
    metadata_country_code: String,
    /// `ServerConfiguration.UICulture` — the culture `GetLocalizedString`
    /// resolves phrases in (en-US fallback), read live so an admin change
    /// applies without a restart, as the C# config fallback does.
    ui_culture: Arc<dyn Fn() -> String + Send + Sync>,
    /// The default country's parental-rating list, built once at construction. It
    /// depends on `metadata_country_code`, so unlike the tables above it cannot be a
    /// process-wide static — but it is still immutable for the manager's lifetime.
    parental_ratings: Vec<ParentalRating>,
}

impl Default for LocalizationManager {
    fn default() -> Self {
        Self::new(DEFAULT_METADATA_COUNTRY_CODE)
    }
}

impl std::fmt::Debug for LocalizationManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalizationManager")
            .field("metadata_country_code", &self.metadata_country_code)
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
        let parental_ratings = build_parental_ratings(&metadata_country_code);
        Self {
            metadata_country_code,
            ui_culture: Arc::new(|| DEFAULT_CULTURE.to_owned()),
            parental_ratings,
        }
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

    /// Port of `GetLocalizedString(phrase)` / `GetServerLocalizedString`: the
    /// phrase in the server's UI culture, falling back to `en-US`, then to the
    /// phrase key itself.
    #[must_use]
    pub fn get_localized_string(&self, phrase: &str) -> String {
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

    /// The available UI-language localization options (C# `GetLocalizationOptions`).
    ///
    /// Jellyfin derives this from the embedded translation-catalog resource files;
    /// that catalog is a `ferrofin-server` asset that this minimal port omits, so
    /// the list is built from the embedded culture dataset's display names
    /// (truncated at the first delimiter), always including the base `en-US`
    /// entry the C# adds explicitly.
    #[must_use]
    pub fn get_localization_options(&self) -> Vec<LocalizationOption> {
        // Port of C# GetLocalizationOptions: a fixed list of UI-language options (the translation
        // catalogs Jellyfin ships), not derived from the culture dataset.
        LOCALIZATION_OPTION_LIST.clone()
    }

    /// Finds the culture matching a language token by display name, name,
    /// three-letter code, or two-letter code (C# `FindLanguageInfo`).
    #[must_use]
    pub fn find_language_info(&self, language: &str) -> Option<&CultureDto> {
        if language.is_empty() {
            return None;
        }
        CULTURE_DATA.cultures.iter().find(|c| {
            language.eq_ignore_ascii_case(&c.display_name)
                || language.eq_ignore_ascii_case(&c.name)
                || language.eq_ignore_ascii_case(&c.two_letter_iso_language_name)
                || c.three_letter_iso_language_names
                    .iter()
                    .any(|n| language.eq_ignore_ascii_case(n))
        })
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
        CULTURE_DATA
            .iso6392_b_to_t
            .get(&iso_b.to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .cloned()
    }

    /// The parental-rating list for the server default country, back-filled with
    /// the common ratings and ordered by score (C# `GetParentalRatings`).
    ///
    /// Built once in [`LocalizationManager::new`]; this is a clone of that list.
    #[must_use]
    pub fn get_parental_ratings(&self) -> Vec<ParentalRating> {
        self.parental_ratings.clone()
    }

    /// Resolves a rating string to a score (C# `GetRatingScore`).
    ///
    /// Handles `/`-separated multi-values (first that resolves wins), unrated
    /// tokens, plain numbers (optionally with a trailing `+`), a `country:rating`
    /// prefix, and a direct lookup in the given (or default) country's table.
    #[must_use]
    pub fn get_rating_score(
        &self,
        rating: &str,
        country_code: Option<&str>,
    ) -> Option<ParentalRatingScore> {
        if rating.is_empty() {
            return None;
        }
        rating
            .split('/')
            .map(str::trim)
            .filter(|r| !r.is_empty())
            .find_map(|value| self.single_rating_score(value, country_code))
    }

    /// Resolves a single (already split) rating value.
    fn single_rating_score(
        &self,
        rating: &str,
        country_code: Option<&str>,
    ) -> Option<ParentalRatingScore> {
        if UNRATED_VALUES
            .iter()
            .any(|u| u.eq_ignore_ascii_case(rating))
        {
            return None;
        }
        if let Some(age) = parse_rating_as_score(rating) {
            return Some(ParentalRatingScore::new(age, None));
        }

        // Strip a leading "Rated " / "Rated:" prefix.
        let cleaned = rating
            .trim_start_matches("Rated :")
            .trim_start_matches("Rated:")
            .trim_start_matches("Rated ")
            .trim();

        let country = country_code.unwrap_or(&self.metadata_country_code);
        if let Some(table) = parental_ratings_for(country)
            && let Some(score) = table.by_name.get(cleaned)
        {
            return Some(*score);
        }

        // Fall back to a scan of every rating system.
        for table in PARENTAL_RATINGS.values() {
            if let Some(score) = table.by_name.get(cleaned) {
                return Some(*score);
            }
        }

        // Try "COUNTRY:rating" / "COUNTRY-rating" prefixes.
        for sep in [':', '-'] {
            if let Some((prefix, suffix)) = cleaned.split_once(sep) {
                let suffix = suffix.trim();
                if suffix.is_empty() {
                    continue;
                }
                if let Some(table) = parental_ratings_for(prefix.trim()) {
                    if let Some(score) = table.by_name.get(suffix) {
                        return Some(*score);
                    }
                    if let Some(age) = parse_rating_as_score(suffix) {
                        return Some(ParentalRatingScore::new(age, None));
                    }
                }
            }
        }
        None
    }
}

/// Parses a rating as a number, allowing a trailing `+` (C# `TryParseRatingAsScore`).
fn parse_rating_as_score(rating: &str) -> Option<i32> {
    rating.trim_end_matches('+').parse::<i32>().ok()
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
        // C# skips a row when the two-letter code or the English name is empty — which is
        // why only the ~200 languages that have an ISO 639-1 code appear in the list.
        if iso639_1.is_empty() || display_name.is_empty() {
            continue;
        }
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
fn load_rating_table(json: &str) -> RatingTable {
    let mut ordered: Vec<(String, ParentalRatingScore)> = Vec::new();
    let mut by_name = HashMap::new();
    if let Ok(system) = serde_json::from_str::<ParentalRatingSystem>(json) {
        for entry in system.ratings {
            if let Some(score) = entry.rating_score {
                for rating in entry.rating_strings {
                    if by_name.insert(rating.clone(), score).is_none() {
                        ordered.push((rating, score));
                    } else if let Some(slot) = ordered.iter_mut().find(|(name, _)| name == &rating)
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

    fn get_localized_string(&self, phrase: &str) -> String {
        LocalizationManager::get_localized_string(self, phrase)
    }

    fn get_localized_string_for(&self, phrase: &str, culture: &str) -> String {
        LocalizationManager::get_localized_string_for(self, phrase, culture)
    }

    fn get_language_display_name(&self, language: &str) -> Option<String> {
        LocalizationManager::get_language_display_name(self, language)
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

    use super::*;

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

    // The full iso6392.txt list is served (Jellyfin's ~200 ISO-639-1 languages), each with
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
            m.get_rating_score("n/a / PG", None).map(|s| s.score),
            Some(10)
        );
        // Country-prefixed.
        assert_eq!(m.get_rating_score("US:R", None).map(|s| s.score), Some(17));
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
        // Countries: the full bundled ISO-3166 set (139), incl. the region codes the old 10-row
        // table lacked (029 = Caribbean, AT = Austria).
        let countries = m.get_countries();
        assert_eq!(countries.len(), 139);
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

        // Localization options: the fixed 71-entry UI-language list, incl. es_419.
        let options = m.get_localization_options();
        assert_eq!(options.len(), 71);
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
        assert_eq!(countries.len(), 139);

        let head: Vec<&str> = countries.iter().take(6).map(|c| c.name.as_str()).collect();
        assert_eq!(head, ["AF", "AL", "DZ", "AR", "AM", "AU"]);
        let tail: Vec<&str> = countries[136..].iter().map(|c| c.name.as_str()).collect();
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
            assert_eq!(table.by_name.get(name), Some(score));
        }
        assert!(parental_ratings_for("zz").is_none());
    }

    /// A non-US default country has no vendored table, so only the back-fill remains —
    /// which is why the rating list cannot be a process-wide static.
    #[test]
    fn parental_ratings_depend_on_the_configured_country() {
        let names: Vec<String> = shipped("DE")
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

    /// The UI-language list keeps its declared order (clients render it as given).
    #[test]
    fn localization_options_preserve_declared_order() {
        let m = shipped(DEFAULT_METADATA_COUNTRY_CODE);
        let options = m.get_localization_options();
        assert_eq!(options.len(), LOCALIZATION_OPTIONS.len());
        let head: Vec<&str> = options.iter().take(4).map(|o| o.value.as_str()).collect();
        assert_eq!(head, ["af", "ar", "be", "bg-BG"]);
        assert_eq!(options.last().map(|o| o.value.as_str()), Some("zh-HK"));
        for (option, (name, value)) in options.iter().zip(LOCALIZATION_OPTIONS) {
            assert_eq!(option.name, *name);
            assert_eq!(option.value, *value);
        }
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
        assert_eq!(cultures.len(), 194);
        let head: Vec<&str> = cultures
            .iter()
            .take(6)
            .map(|c| c.two_letter_iso_language_name.as_str())
            .collect();
        // File order (ISO 639-2/T), which is *not* two-letter-code order: "af" precedes "ak"
        // and "am", but "ar" trails them.
        assert_eq!(head, ["aa", "ab", "af", "ak", "am", "ar"]);
        assert_eq!(cultures[0].display_name, "Afar");
        assert_eq!(
            cultures.last().map(|c| c.display_name.as_str()),
            Some("Zulu")
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
            assert_eq!(table.by_name.get(name), Some(score), "{name}");
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
}
