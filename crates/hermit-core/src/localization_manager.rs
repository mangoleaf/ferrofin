//! [`LocalizationManager`] — a concrete culture/country/parental-rating service.
//!
//! Port of `Emby.Server.Implementations.Localization.LocalizationManager`, cut
//! down to the culture-data surface that the rest of Hermit needs today.
//!
//! **Flagged for `hermit-traits`:** the C# `ILocalizationManager` interface was
//! deliberately *not* ported into `hermit-traits` (its module doc says the
//! service is "not part of the wire contract, so it is dropped"). Consumers of
//! this data (the DTO layer's language display names, the metadata provider's
//! rating scores) will need a trait to inject it. This unit ships a **concrete
//! struct**; a follow-up should add an `ILocalizationManager` trait to
//! `hermit-traits` and have this type implement it. The public methods here are
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
//! the runtime translation catalog is a `hermit-server` asset concern.

use std::collections::HashMap;

use hermit_model::entities_media::{ParentalRating, ParentalRatingScore};
use hermit_model::globalization::{CountryInfo, CultureDto, LocalizationOption};

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

/// A concrete culture/country/parental-rating service over a minimal dataset.
///
/// See the module docs: this stands in for the (unported) C#
/// `ILocalizationManager` and should be fronted by a `hermit-traits` trait in a
/// follow-up.
#[derive(Debug, Clone)]
pub struct LocalizationManager {
    cultures: Vec<CultureDto>,
    /// ISO 639-2/B → /T (C# `_iso6392BtoT`).
    iso6392_b_to_t: HashMap<String, String>,
    /// Country code (upper-case) → rating string → score.
    parental_ratings: HashMap<String, HashMap<String, ParentalRatingScore>>,
    /// The server default country code for rating fallback.
    metadata_country_code: String,
}

impl Default for LocalizationManager {
    fn default() -> Self {
        Self::new(DEFAULT_METADATA_COUNTRY_CODE)
    }
}

impl LocalizationManager {
    /// Builds the manager over the embedded dataset, using `metadata_country_code`
    /// (e.g. `"US"`) as the default for rating lookups.
    #[must_use]
    #[allow(clippy::similar_names)] // iso639_2t / iso639_2b are the standard ISO 639-2 column names
    pub fn new(metadata_country_code: &str) -> Self {
        let mut cultures = Vec::new();
        let mut iso6392_b_to_t = HashMap::new();
        // Port of C# `LoadCultures`: parse iso6392.txt (`T|B|1|EnglishName|FrenchName`).
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

        // Load the US rating system from the vendored Ratings/us.json, expanding each entry's
        // ratingStrings to share its score+subScore (C# LoadAll ratings loop). Keyed upper-case
        // to match parental_ratings_for's lookup.
        let mut us = HashMap::new();
        if let Ok(system) = serde_json::from_str::<ParentalRatingSystem>(RATINGS_US_JSON) {
            for entry in system.ratings {
                if let Some(score) = entry.rating_score {
                    for rating in entry.rating_strings {
                        us.insert(rating, score);
                    }
                }
            }
        }
        let mut parental_ratings = HashMap::new();
        parental_ratings.insert("US".to_owned(), us);

        Self {
            cultures,
            iso6392_b_to_t,
            parental_ratings,
            metadata_country_code: metadata_country_code.to_ascii_uppercase(),
        }
    }

    /// All known cultures (C# `GetCultures`).
    #[must_use]
    pub fn get_cultures(&self) -> &[CultureDto] {
        &self.cultures
    }

    /// All known countries (C# `GetCountries`).
    #[must_use]
    pub fn get_countries(&self) -> Vec<CountryInfo> {
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
    }

    /// The available UI-language localization options (C# `GetLocalizationOptions`).
    ///
    /// Jellyfin derives this from the embedded translation-catalog resource files;
    /// that catalog is a `hermit-server` asset that this minimal port omits, so
    /// the list is built from the embedded culture dataset's display names
    /// (truncated at the first delimiter), always including the base `en-US`
    /// entry the C# adds explicitly.
    #[must_use]
    pub fn get_localization_options(&self) -> Vec<LocalizationOption> {
        // Port of C# GetLocalizationOptions: a fixed list of UI-language options (the translation
        // catalogs Jellyfin ships), not derived from the culture dataset.
        LOCALIZATION_OPTIONS
            .iter()
            .map(|(name, value)| LocalizationOption {
                name: (*name).to_owned(),
                value: (*value).to_owned(),
            })
            .collect()
    }

    /// Finds the culture matching a language token by display name, name,
    /// three-letter code, or two-letter code (C# `FindLanguageInfo`).
    #[must_use]
    pub fn find_language_info(&self, language: &str) -> Option<&CultureDto> {
        if language.is_empty() {
            return None;
        }
        self.cultures.iter().find(|c| {
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
        self.iso6392_b_to_t
            .get(&iso_b.to_ascii_lowercase())
            .filter(|t| !t.is_empty())
            .cloned()
    }

    /// The parental-rating list for the server default country, back-filled with
    /// the common ratings and ordered by score (C# `GetParentalRatings`).
    #[must_use]
    pub fn get_parental_ratings(&self) -> Vec<ParentalRating> {
        let mut ratings: Vec<ParentalRating> = self
            .parental_ratings_for(&self.metadata_country_code)
            .map(|dict| {
                dict.iter()
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

    /// The rating dictionary for a country code (case-insensitive).
    fn parental_ratings_for(
        &self,
        country_code: &str,
    ) -> Option<&HashMap<String, ParentalRatingScore>> {
        self.parental_ratings
            .get(&country_code.to_ascii_uppercase())
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
        if let Some(dict) = self.parental_ratings_for(country)
            && let Some(score) = dict.get(cleaned)
        {
            return Some(*score);
        }

        // Fall back to a scan of every rating system.
        for dict in self.parental_ratings.values() {
            if let Some(score) = dict.get(cleaned) {
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
                if let Some(dict) = self.parental_ratings_for(prefix.trim()) {
                    if let Some(score) = dict.get(suffix) {
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

impl hermit_traits::localization::LocalizationManager for LocalizationManager {
    fn get_cultures(&self) -> Vec<CultureDto> {
        self.cultures.clone()
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

    fn get_rating_score(
        &self,
        rating: &str,
        country_code: Option<&str>,
    ) -> Option<ParentalRatingScore> {
        LocalizationManager::get_rating_score(self, rating, country_code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
