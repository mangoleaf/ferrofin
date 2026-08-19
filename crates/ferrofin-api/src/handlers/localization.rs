//! `LocalizationController` — culture / country / rating reference data.
//!
//! Ports the four read-only `[Authorize(FirstTimeSetupOrDefault)]` actions:
//! - `GET /Localization/Cultures` — the known cultures, de-duplicated by display
//!   name (case-insensitive) and ordered by it (case-insensitively — see
//!   [`distinct_ordered_cultures`]).
//! - `GET /Localization/Countries` — the known countries.
//! - `GET /Localization/ParentalRatings` — the known parental ratings.
//! - `GET /Localization/Options` — the UI localization options.
//!
//! All delegate to the [`LocalizationManager`](ferrofin_traits::localization::LocalizationManager).

use std::collections::HashSet;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use ferrofin_model::entities_media::ParentalRating;
use ferrofin_model::globalization::{CountryInfo, CultureDto, LocalizationOption};

use crate::auth::FirstTimeSetupOrAuth;
use crate::state::AppState;

/// `GET /Localization/Cultures` — the known cultures, distinct + ordered.
///
/// Port of `LocalizationController.GetCultures`: de-duplicates by display name
/// (case-insensitive, keeping the first) and orders by display name.
#[utoipa::path(
    get,
    path = "/Localization/Cultures",
    responses((status = 200, description = "Known cultures returned", body = [CultureDto])),
    tag = "ferrofin"
)]
async fn get_cultures(
    State(state): State<AppState>,
    _auth: FirstTimeSetupOrAuth,
) -> Json<Vec<CultureDto>> {
    Json(distinct_ordered_cultures(state.localization.get_cultures()))
}

/// C#'s `cultures.DistinctBy(c => c.DisplayName, StringComparer.OrdinalIgnoreCase)
/// .OrderBy(c => c.DisplayName)`, shared by `GET /Localization/Cultures` and
/// `GET /Items/{itemId}/MetadataEditor` so the two lists a client cross-references
/// are ordered identically.
///
/// Port note — the comparer: `OrderBy(string)` with no comparer uses
/// `Comparer<string>.Default`, i.e. `string.CompareTo` /
/// `StringComparer.CurrentCulture` — a *linguistic* comparison, not an ordinal
/// one. Case is only a tertiary difference there, so "afar" sorts before "Zulu"
/// (ordinal would put "Zulu" first, since `'Z' (0x5A) < 'a' (0x61)`). Case is
/// therefore folded for the sort key. Case *ties* need no ordinal tiebreak: names
/// differing only by case were already collapsed by the `OrdinalIgnoreCase`
/// dedupe, and the stable sort keeps the surviving source order for the rest.
///
/// The dedupe runs first and keeps the first occurrence in *source* order, as
/// `DistinctBy` does — which entry survives decides the `Name` /
/// `ThreeLetterISOLanguageName` the client sees for that display name.
pub(crate) fn distinct_ordered_cultures(cultures: Vec<CultureDto>) -> Vec<CultureDto> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut cultures: Vec<CultureDto> = cultures
        .into_iter()
        .filter(|c| seen.insert(c.display_name.to_ascii_lowercase()))
        .collect();
    cultures.sort_by_cached_key(|c| c.display_name.to_ascii_lowercase());
    cultures
}

/// `GET /Localization/Countries` — the known countries.
///
/// Port of `LocalizationController.GetCountries`.
#[utoipa::path(
    get,
    path = "/Localization/Countries",
    responses((status = 200, description = "Known countries returned", body = [CountryInfo])),
    tag = "ferrofin"
)]
async fn get_countries(
    State(state): State<AppState>,
    _auth: FirstTimeSetupOrAuth,
) -> Json<Vec<CountryInfo>> {
    Json(state.localization.get_countries())
}

/// `GET /Localization/ParentalRatings` — the known parental ratings.
///
/// Port of `LocalizationController.GetParentalRatings`.
#[utoipa::path(
    get,
    path = "/Localization/ParentalRatings",
    responses((status = 200, description = "Known parental ratings returned", body = [ParentalRating])),
    tag = "ferrofin"
)]
async fn get_parental_ratings(
    State(state): State<AppState>,
    _auth: FirstTimeSetupOrAuth,
) -> Json<Vec<ParentalRating>> {
    Json(state.localization.get_parental_ratings())
}

/// `GET /Localization/Options` — the UI localization options.
///
/// Port of `LocalizationController.GetLocalizationOptions`.
#[utoipa::path(
    get,
    path = "/Localization/Options",
    responses((status = 200, description = "Localization options returned", body = [LocalizationOption])),
    tag = "ferrofin"
)]
async fn get_localization_options(
    State(state): State<AppState>,
    _auth: FirstTimeSetupOrAuth,
) -> Json<Vec<LocalizationOption>> {
    Json(state.localization.get_localization_options())
}

/// Registers this controller's real routes onto `router`.
pub fn register(router: Router<AppState>) -> Router<AppState> {
    router
        .route("/Localization/Cultures", get(get_cultures))
        .route("/Localization/Countries", get(get_countries))
        .route("/Localization/ParentalRatings", get(get_parental_ratings))
        .route("/Localization/Options", get(get_localization_options))
}
