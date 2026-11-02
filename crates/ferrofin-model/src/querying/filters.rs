//! `QueryFilters` and `QueryFiltersLegacy` — port of the matching types in
//! `MediaBrowser.Model.Querying`.
//!
//! `QueryFilters` carries the Jellyfin 12+ shape (v12.2
//! `MediaBrowser.Model/Querying/QueryFilters.cs`): genres, tags and the audio
//! and subtitle language facets. The language facets live only here — the
//! legacy flat-string set is unchanged from 10.11.

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

    /// Gets or sets the audio languages (Jellyfin 12+): a `{Name, Value}`
    /// pair per distinct audio-stream language code.
    pub audio_languages: Vec<NameValuePair>,

    /// Gets or sets the subtitle languages (Jellyfin 12+): a `{Name, Value}`
    /// pair per distinct subtitle-stream language code.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_filters_serializes_all_four_facets_as_arrays_in_order() {
        // The C# constructor sets every facet to an empty array, so none is
        // ever null or omitted — and the property order is the class order.
        // (Asserted on the text: `serde_json::Value` sorts its keys.)
        let json = serde_json::to_string(&QueryFilters::default()).expect("serialize");
        assert_eq!(
            json,
            r#"{"Genres":[],"Tags":[],"AudioLanguages":[],"SubtitleLanguages":[]}"#
        );
    }

    #[test]
    fn query_filters_round_trips_language_pairs() {
        let filters = QueryFilters {
            audio_languages: vec![NameValuePair::new("English (eng)", "eng")],
            subtitle_languages: vec![NameValuePair::new("French (fra)", "fra")],
            ..QueryFilters::default()
        };
        let json = serde_json::to_value(&filters).expect("serialize");
        assert_eq!(
            json["AudioLanguages"],
            serde_json::json!([{ "Name": "English (eng)", "Value": "eng" }])
        );
        assert_eq!(
            json["SubtitleLanguages"],
            serde_json::json!([{ "Name": "French (fra)", "Value": "fra" }])
        );
        let back: QueryFilters = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, filters);
    }

    #[test]
    fn query_filters_legacy_carries_no_language_facets() {
        let json = serde_json::to_string(&QueryFiltersLegacy::default()).expect("serialize");
        assert_eq!(
            json,
            r#"{"Genres":[],"Tags":[],"OfficialRatings":[],"Years":[]}"#
        );
    }
}
