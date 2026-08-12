//! Localization trait — the DI seam over the culture/country/rating dataset.
//!
//! Port of `MediaBrowser.Model.Globalization.ILocalizationManager`, cut to the
//! read-only, wire-facing surface the `LocalizationController` needs plus the
//! rating-resolution helpers other subsystems consume. The concrete dataset
//! lives in `ferrofin-core`; handlers depend only on this trait so the data source
//! stays swappable and the API crate never names `ferrofin-core`.
//!
//! Every method is synchronous (the data is process-static, embedded at build
//! time) and the trait is object-safe.

use ferrofin_model::entities_media::{ParentalRating, ParentalRatingScore};
use ferrofin_model::globalization::{CountryInfo, CultureDto, LocalizationOption};

/// Provides culture, country, and parental-rating reference data.
///
/// Port of `ILocalizationManager` (read-only subset).
pub trait LocalizationManager: Send + Sync {
    /// All known cultures (C# `GetCultures`).
    fn get_cultures(&self) -> Vec<CultureDto>;

    /// All known countries (C# `GetCountries`).
    fn get_countries(&self) -> Vec<CountryInfo>;

    /// The parental ratings for the server's default country, back-filled with
    /// the common ratings and ordered by score (C# `GetParentalRatings`).
    fn get_parental_ratings(&self) -> Vec<ParentalRating>;

    /// The list of localization (UI-language) options (C# `GetLocalizationOptions`).
    fn get_localization_options(&self) -> Vec<LocalizationOption>;

    /// Resolves a rating string to a score, honouring the given country code
    /// (C# `GetRatingScore`).
    fn get_rating_score(
        &self,
        rating: &str,
        country_code: Option<&str>,
    ) -> Option<ParentalRatingScore>;
}

fn _assert_object_safe_localization_manager(_: &dyn LocalizationManager) {}
