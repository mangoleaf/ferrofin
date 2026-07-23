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

/// One row of the embedded ISO 639 culture table.
///
/// `(iso639_2t, iso639_2b, iso639_1, display_name)` — the same four fields C#
/// reads out of `iso6392.txt` (columns 0,1,2,3). An empty `iso639_2b` means the
/// B and T codes coincide.
struct CultureRow {
    iso639_2t: &'static str,
    iso639_2b: &'static str,
    iso639_1: &'static str,
    display_name: &'static str,
}

/// The compact embedded culture dataset (common media languages).
const CULTURE_ROWS: &[CultureRow] = &[
    CultureRow {
        iso639_2t: "eng",
        iso639_2b: "",
        iso639_1: "en",
        display_name: "English",
    },
    CultureRow {
        iso639_2t: "spa",
        iso639_2b: "",
        iso639_1: "es",
        display_name: "Spanish; Castilian",
    },
    CultureRow {
        iso639_2t: "fra",
        iso639_2b: "fre",
        iso639_1: "fr",
        display_name: "French",
    },
    CultureRow {
        iso639_2t: "deu",
        iso639_2b: "ger",
        iso639_1: "de",
        display_name: "German",
    },
    CultureRow {
        iso639_2t: "ita",
        iso639_2b: "",
        iso639_1: "it",
        display_name: "Italian",
    },
    CultureRow {
        iso639_2t: "por",
        iso639_2b: "",
        iso639_1: "pt",
        display_name: "Portuguese",
    },
    CultureRow {
        iso639_2t: "nld",
        iso639_2b: "dut",
        iso639_1: "nl",
        display_name: "Dutch; Flemish",
    },
    CultureRow {
        iso639_2t: "rus",
        iso639_2b: "",
        iso639_1: "ru",
        display_name: "Russian",
    },
    CultureRow {
        iso639_2t: "jpn",
        iso639_2b: "",
        iso639_1: "ja",
        display_name: "Japanese",
    },
    CultureRow {
        iso639_2t: "kor",
        iso639_2b: "",
        iso639_1: "ko",
        display_name: "Korean",
    },
    CultureRow {
        iso639_2t: "zho",
        iso639_2b: "chi",
        iso639_1: "zh",
        display_name: "Chinese",
    },
    CultureRow {
        iso639_2t: "ara",
        iso639_2b: "",
        iso639_1: "ar",
        display_name: "Arabic",
    },
    CultureRow {
        iso639_2t: "hin",
        iso639_2b: "",
        iso639_1: "hi",
        display_name: "Hindi",
    },
    CultureRow {
        iso639_2t: "swe",
        iso639_2b: "",
        iso639_1: "sv",
        display_name: "Swedish",
    },
    CultureRow {
        iso639_2t: "nor",
        iso639_2b: "",
        iso639_1: "no",
        display_name: "Norwegian",
    },
    CultureRow {
        iso639_2t: "dan",
        iso639_2b: "",
        iso639_1: "da",
        display_name: "Danish",
    },
    CultureRow {
        iso639_2t: "fin",
        iso639_2b: "",
        iso639_1: "fi",
        display_name: "Finnish",
    },
    CultureRow {
        iso639_2t: "pol",
        iso639_2b: "",
        iso639_1: "pl",
        display_name: "Polish",
    },
    CultureRow {
        iso639_2t: "tur",
        iso639_2b: "",
        iso639_1: "tr",
        display_name: "Turkish",
    },
    CultureRow {
        iso639_2t: "ces",
        iso639_2b: "cze",
        iso639_1: "cs",
        display_name: "Czech",
    },
];

/// The compact embedded country dataset (common metadata regions).
/// `(name, two-letter, three-letter, display)`.
const COUNTRY_ROWS: &[(&str, &str, &str, &str)] = &[
    ("US", "US", "USA", "United States"),
    ("GB", "GB", "GBR", "United Kingdom"),
    ("CA", "CA", "CAN", "Canada"),
    ("AU", "AU", "AUS", "Australia"),
    ("DE", "DE", "DEU", "Germany"),
    ("FR", "FR", "FRA", "France"),
    ("ES", "ES", "ESP", "Spain"),
    ("IT", "IT", "ITA", "Italy"),
    ("JP", "JP", "JPN", "Japan"),
    ("NL", "NL", "NLD", "Netherlands"),
];

/// The US parental rating system (rating string → score). Mirrors the entries
/// Jellyfin ships in `Ratings/us.json` for the common film/TV ratings.
const US_RATINGS: &[(&str, i32)] = &[
    ("G", 0),
    ("TV-G", 0),
    ("TV-Y", 0),
    ("PG", 10),
    ("TV-PG", 10),
    ("TV-Y7", 7),
    ("PG-13", 13),
    ("TV-14", 14),
    ("R", 17),
    ("NC-17", 18),
    ("TV-MA", 17),
];

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
    pub fn new(metadata_country_code: &str) -> Self {
        let mut cultures = Vec::with_capacity(CULTURE_ROWS.len());
        let mut iso6392_b_to_t = HashMap::new();
        for row in CULTURE_ROWS {
            // Match C#: the display name uses column 3; the "name" is the
            // two-letter code when present.
            let name = if row.iso639_1.is_empty() {
                row.iso639_2t.to_owned()
            } else {
                row.iso639_1.to_owned()
            };
            let three_letter_names = if row.iso639_2b.is_empty() {
                vec![row.iso639_2t.to_owned()]
            } else {
                // Record the B→T mapping (C# `iso6392BtoTdict.TryAdd`).
                iso6392_b_to_t
                    .entry(row.iso639_2b.to_ascii_lowercase())
                    .or_insert_with(|| row.iso639_2t.to_owned());
                vec![row.iso639_2t.to_owned(), row.iso639_2b.to_owned()]
            };
            cultures.push(CultureDto {
                name,
                display_name: row.display_name.to_owned(),
                two_letter_iso_language_name: row.iso639_1.to_owned(),
                three_letter_iso_language_name: three_letter_names.first().cloned(),
                three_letter_iso_language_names: three_letter_names,
            });
        }

        let mut us = HashMap::new();
        for (rating, score) in US_RATINGS {
            us.insert((*rating).to_owned(), ParentalRatingScore::new(*score, None));
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
        COUNTRY_ROWS
            .iter()
            .map(|(name, two, three, display)| CountryInfo {
                name: (*name).to_owned(),
                display_name: (*display).to_owned(),
                two_letter_iso_region_name: (*two).to_owned(),
                three_letter_iso_region_name: (*three).to_owned(),
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
        let mut options = vec![LocalizationOption {
            name: "English".to_owned(),
            value: "en-US".to_owned(),
        }];
        for culture in &self.cultures {
            if culture.two_letter_iso_language_name.is_empty()
                || culture.two_letter_iso_language_name == "en"
            {
                continue;
            }
            let name = culture
                .display_name
                .split([';', ','])
                .next()
                .unwrap_or(&culture.display_name)
                .trim()
                .to_owned();
            options.push(LocalizationOption {
                name,
                value: culture.two_letter_iso_language_name.clone(),
            });
        }
        options
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
        assert_eq!(by_display.name, "de");
        assert!(m.find_language_info("zzz").is_none());
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
}
