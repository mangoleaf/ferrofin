//! Port of the portable DTOs in `MediaBrowser.Model.Globalization`.
//!
//! The `ILocalizationManager` service interface is a server-side manager and is
//! not part of the wire contract, so it is dropped from this port.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Class `CountryInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct CountryInfo {
    /// Gets or sets the name.
    pub name: String,

    /// Gets or sets the display name.
    pub display_name: String,

    /// Gets or sets the name of the two letter ISO region.
    #[serde(rename = "TwoLetterISORegionName")]
    pub two_letter_iso_region_name: String,

    /// Gets or sets the name of the three letter ISO region.
    #[serde(rename = "ThreeLetterISORegionName")]
    pub three_letter_iso_region_name: String,
}

/// Class `CultureDto`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct CultureDto {
    /// Gets the name.
    pub name: String,

    /// Gets the display name.
    pub display_name: String,

    /// Gets the name of the two letter ISO language.
    #[serde(rename = "TwoLetterISOLanguageName")]
    pub two_letter_iso_language_name: String,

    /// Gets the name of the three letter ISO language (first of the list).
    #[serde(
        rename = "ThreeLetterISOLanguageName",
        skip_serializing_if = "Option::is_none"
    )]
    pub three_letter_iso_language_name: Option<String>,

    /// Gets the names of the three letter ISO languages.
    #[serde(rename = "ThreeLetterISOLanguageNames")]
    pub three_letter_iso_language_names: Vec<String>,
}

/// A localization option (a name/value pair).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LocalizationOption {
    /// Gets or sets the name.
    pub name: String,

    /// Gets or sets the value.
    pub value: String,
}
