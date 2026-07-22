//! `QueryFilters` and `QueryFiltersLegacy` — port of the matching types in
//! `MediaBrowser.Model.Querying`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::dto::{NameGuidPair, NameValuePair};

/// The available filter facets for a query.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct QueryFilters {
    /// Gets or sets the genres.
    pub genres: Vec<NameGuidPair>,

    /// Gets or sets the tags.
    pub tags: Vec<String>,

    /// Gets or sets the audio languages.
    pub audio_languages: Vec<NameValuePair>,

    /// Gets or sets the subtitle languages.
    pub subtitle_languages: Vec<NameValuePair>,
}

/// The legacy (flat-string) filter facets for a query.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct QueryFiltersLegacy {
    /// Gets or sets the genres.
    pub genres: Vec<String>,

    /// Gets or sets the tags.
    pub tags: Vec<String>,

    /// Gets or sets the official ratings.
    pub official_ratings: Vec<String>,

    /// Gets or sets the years.
    pub years: Vec<i32>,
}
