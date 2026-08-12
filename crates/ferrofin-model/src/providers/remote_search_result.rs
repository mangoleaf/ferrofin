//! `RemoteSearchResult` — port of
//! `MediaBrowser.Model.Providers.RemoteSearchResult`.

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::entities_media::IHasProviderIds;

/// A candidate result from a remote metadata search.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteSearchResult {
    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the provider ids.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_ids: Option<HashMap<String, String>>,

    /// Gets or sets the production year.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_year: Option<i32>,

    /// Gets or sets the index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number: Option<i32>,

    /// Gets or sets the end index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number_end: Option<i32>,

    /// Gets or sets the parent index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_index_number: Option<i32>,

    /// Gets or sets the premiere date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
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

impl IHasProviderIds for RemoteSearchResult {
    fn provider_ids(&self) -> Option<&HashMap<String, String>> {
        self.provider_ids.as_ref()
    }

    fn provider_ids_mut(&mut self) -> &mut HashMap<String, String> {
        self.provider_ids.get_or_insert_with(HashMap::new)
    }

    fn provider_ids_opt_mut(&mut self) -> &mut Option<HashMap<String, String>> {
        &mut self.provider_ids
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> RemoteSearchResult {
        let mut ids = HashMap::new();
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

    #[test]
    fn provider_ids_trait_accessors() {
        let mut value = RemoteSearchResult::default();
        assert!(value.provider_ids().is_none());
        value
            .provider_ids_mut()
            .insert("Tmdb".to_owned(), "278".to_owned());
        assert_eq!(value.provider_ids().unwrap()["Tmdb"], "278");
        *value.provider_ids_opt_mut() = None;
        assert!(value.provider_ids().is_none());
    }
}
