//! `RemoteSearchResult` — port of
//! `MediaBrowser.Model.Providers.RemoteSearchResult`.

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// A candidate result from a remote metadata search.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteSearchResult {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the provider ids.
    // In the order they were set or posted: upstream's `Dictionary<string,
    // string>` keeps insertion order, which decides the value `SetProviderIds`
    // keeps of two spellings of one name (gl-2028).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<std::collections::HashMap<String, String>>)]
    pub provider_ids: Option<IndexMap<String, String>>,

    /// Gets or sets the production year.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "crate::json::value::nullable")]
    pub production_year: Option<i32>,

    /// Gets or sets the index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "crate::json::value::nullable")]
    pub index_number: Option<i32>,

    /// Gets or sets the end index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "crate::json::value::nullable")]
    pub index_number_end: Option<i32>,

    /// Gets or sets the parent index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[serde(default, deserialize_with = "crate::json::value::nullable")]
    pub parent_index_number: Option<i32>,

    /// Gets or sets the premiere date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub premiere_date: Option<DateTime<Utc>>,

    /// Gets or sets the image url.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_url: Option<String>,

    /// Gets or sets the search provider name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_provider_name: Option<String>,

    /// Gets or sets the overview.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub overview: Option<String>,

    /// Gets or sets the album artist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<Box<RemoteSearchResult>>,

    /// Gets or sets the artists.
    #[serde(default)]
    pub artists: Vec<RemoteSearchResult>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RemoteSearchResult {
        let mut ids = IndexMap::new();
        ids.insert("Imdb".to_owned(), "tt0111161".to_owned());
        RemoteSearchResult {
            name: Some("The Shawshank Redemption".to_owned()),
            provider_ids: Some(ids),
            production_year: Some(1994),
            search_provider_name: Some("TheMovieDb".to_owned()),
            artists: vec![RemoteSearchResult {
                name: Some("Some Artist".to_owned()),
                ..RemoteSearchResult::default()
            }],
            album_artist: Some(Box::new(RemoteSearchResult {
                name: Some("Album Artist".to_owned()),
                ..RemoteSearchResult::default()
            })),
            ..RemoteSearchResult::default()
        }
    }

    #[test]
    fn round_trips_through_json() {
        let value = sample();
        let json = serde_json::to_string(&value).unwrap();
        let back: RemoteSearchResult = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn uses_contract_field_names() {
        let json = serde_json::to_value(sample()).unwrap();
        assert_eq!(json["Name"], "The Shawshank Redemption");
        assert_eq!(json["ProductionYear"], 1994);
        assert_eq!(json["SearchProviderName"], "TheMovieDb");
        assert_eq!(json["ProviderIds"]["Imdb"], "tt0111161");
    }

    #[test]
    fn default_omits_optional_fields() {
        let json = serde_json::to_value(RemoteSearchResult::default()).unwrap();
        assert!(json.get("Name").is_none());
        assert!(json.get("ProviderIds").is_none());
        // Non-optional collections are still emitted.
        assert_eq!(json["Artists"], serde_json::json!([]));
    }

    /// gl-2028: the ids keep the order they were posted in, as upstream's
    /// `Dictionary<string, string>` does, in and out.
    #[test]
    fn provider_ids_keep_their_document_order() {
        let value: RemoteSearchResult =
            serde_json::from_str(r#"{"ProviderIds":{"zz":"1","Tmdb":"2","ZZ":"3","aa":"4"}}"#)
                .unwrap();
        let keys: Vec<&str> = value
            .provider_ids
            .as_ref()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(keys, ["zz", "Tmdb", "ZZ", "aa"]);
        assert_eq!(
            serde_json::to_string(&value).unwrap(),
            r#"{"ProviderIds":{"zz":"1","Tmdb":"2","ZZ":"3","aa":"4"},"Artists":[]}"#
        );
    }
}
