//! Port of `MediaBrowser.Model.Search`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::data::{BaseItemKind, MediaType};

/// A single search-hint result.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SearchHint {
    /// Gets or sets the item id (deprecated; use [`Self::id`]).
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub item_id: Uuid,

    /// Gets or sets the item id.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub id: Uuid,

    /// Gets or sets the name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Gets or sets the matched term.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub matched_term: Option<String>,

    /// Gets or sets the index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_number: Option<i32>,

    /// Gets or sets the production year.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub production_year: Option<i32>,

    /// Gets or sets the parent index number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_index_number: Option<i32>,

    /// Gets or sets the primary image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_image_tag: Option<String>,

    /// Gets or sets the thumb image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_image_tag: Option<String>,

    /// Gets or sets the thumb image item identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thumb_image_item_id: Option<String>,

    /// Gets or sets the backdrop image tag.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backdrop_image_tag: Option<String>,

    /// Gets or sets the backdrop image item identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub backdrop_image_item_id: Option<String>,

    /// Gets or sets the type.
    #[serde(rename = "Type")]
    pub type_: BaseItemKind,

    /// Gets or sets a value indicating whether this instance is a folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_folder: Option<bool>,

    /// Gets or sets the run time ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_time_ticks: Option<i64>,

    /// Gets or sets the type of the media.
    pub media_type: MediaType,

    /// Gets or sets the start date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub start_date: Option<DateTime<Utc>>,

    /// Gets or sets the end date.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "date-time")]
    #[serde(default, with = "crate::json::datetime::option")]
    pub end_date: Option<DateTime<Utc>>,

    /// Gets or sets the series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series: Option<String>,

    /// Gets or sets the status.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,

    /// Gets or sets the album.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,

    /// Gets or sets the album id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub album_id: Option<Uuid>,

    /// Gets or sets the album artist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artist: Option<String>,

    /// Gets or sets the artists.
    pub artists: Vec<String>,

    /// Gets or sets the song count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub song_count: Option<i32>,

    /// Gets or sets the episode count.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_count: Option<i32>,

    /// Gets or sets the channel identifier.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub channel_id: Option<Uuid>,

    /// Gets or sets the name of the channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_name: Option<String>,

    /// Gets or sets the primary image aspect ratio.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub primary_image_aspect_ratio: Option<f64>,
}

/// The result of a search-hint query.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct SearchHintResult {
    /// Gets the search hints.
    pub search_hints: Vec<SearchHint>,

    /// Gets the total record count.
    pub total_record_count: i32,
}

impl SearchHintResult {
    /// Initializes a new [`SearchHintResult`].
    #[must_use]
    pub fn new(search_hints: Vec<SearchHint>, total_record_count: i32) -> Self {
        Self {
            search_hints,
            total_record_count,
        }
    }
}

/// A query for search hints.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct SearchQuery {
    /// Gets or sets the user to localize search results for.
    #[schema(value_type = String, format = "uuid")]
    #[serde(with = "crate::json::guid")]
    pub user_id: Uuid,

    /// Gets or sets the search term.
    pub search_term: String,

    /// Gets or sets the start index. Used for paging.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_index: Option<i32>,

    /// Gets or sets the maximum number of items to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<i32>,

    /// Gets or sets a value indicating whether to include people.
    pub include_people: bool,

    /// Gets or sets a value indicating whether to include media.
    pub include_media: bool,

    /// Gets or sets a value indicating whether to include genres.
    pub include_genres: bool,

    /// Gets or sets a value indicating whether to include studios.
    pub include_studios: bool,

    /// Gets or sets a value indicating whether to include artists.
    pub include_artists: bool,

    /// Gets or sets the media types.
    pub media_types: Vec<MediaType>,

    /// Gets or sets the include item types.
    pub include_item_types: Vec<BaseItemKind>,

    /// Gets or sets the exclude item types.
    pub exclude_item_types: Vec<BaseItemKind>,

    /// Gets or sets the parent id.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<String>, format = "uuid")]
    #[serde(default, with = "crate::json::guid::option")]
    pub parent_id: Option<Uuid>,

    /// Gets or sets a value indicating whether the item is a movie.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_movie: Option<bool>,

    /// Gets or sets a value indicating whether the item is a series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_series: Option<bool>,

    /// Gets or sets a value indicating whether the item is news.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_news: Option<bool>,

    /// Gets or sets a value indicating whether the item is kids content.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_kids: Option<bool>,

    /// Gets or sets a value indicating whether the item is sports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_sports: Option<bool>,
}

impl Default for SearchQuery {
    fn default() -> Self {
        Self {
            user_id: Uuid::nil(),
            search_term: String::new(),
            start_index: None,
            limit: None,
            include_people: true,
            include_media: true,
            include_genres: true,
            include_studios: true,
            include_artists: true,
            media_types: Vec::new(),
            include_item_types: Vec::new(),
            exclude_item_types: Vec::new(),
            parent_id: None,
            is_movie: None,
            is_series: None,
            is_news: None,
            is_kids: None,
            is_sports: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_hint() -> SearchHint {
        SearchHint {
            item_id: Uuid::from_u128(1),
            id: Uuid::from_u128(1),
            name: Some("Result".to_owned()),
            matched_term: Some("res".to_owned()),
            index_number: None,
            production_year: Some(2020),
            parent_index_number: None,
            primary_image_tag: None,
            thumb_image_tag: None,
            thumb_image_item_id: None,
            backdrop_image_tag: None,
            backdrop_image_item_id: None,
            type_: BaseItemKind::default(),
            is_folder: Some(false),
            run_time_ticks: Some(1_000),
            media_type: MediaType::Video,
            start_date: None,
            end_date: None,
            series: None,
            status: None,
            album: None,
            album_id: None,
            album_artist: None,
            artists: vec!["Artist".to_owned()],
            song_count: None,
            episode_count: None,
            channel_id: None,
            channel_name: None,
            primary_image_aspect_ratio: Some(1.78),
        }
    }

    #[test]
    fn search_hint_round_trips() {
        let value = sample_hint();
        let json = serde_json::to_string(&value).unwrap();
        let back: SearchHint = serde_json::from_str(&json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn search_hint_uses_contract_field_names() {
        let json = serde_json::to_value(sample_hint()).unwrap();
        assert_eq!(json["ItemId"], Uuid::from_u128(1).simple().to_string());
        assert_eq!(json["Id"], Uuid::from_u128(1).simple().to_string());
        assert_eq!(json["Name"], "Result");
        assert_eq!(
            json["Type"],
            serde_json::to_value(BaseItemKind::default()).unwrap()
        );
        assert_eq!(json["MediaType"], "Video");
        assert_eq!(json["Artists"], serde_json::json!(["Artist"]));
    }

    #[test]
    fn search_hint_result_new_and_round_trip() {
        let result = SearchHintResult::new(vec![sample_hint()], 1);
        assert_eq!(result.total_record_count, 1);
        assert_eq!(result.search_hints.len(), 1);
        let json = serde_json::to_value(&result).unwrap();
        assert_eq!(json["TotalRecordCount"], 1);
        let back: SearchHintResult = serde_json::from_value(json).unwrap();
        assert_eq!(result, back);
    }

    #[test]
    fn search_query_default_flags() {
        let query = SearchQuery::default();
        assert_eq!(query.user_id, Uuid::nil());
        assert!(query.include_people);
        assert!(query.include_media);
        assert!(query.include_genres);
        assert!(query.include_studios);
        assert!(query.include_artists);
    }

    #[test]
    fn search_query_round_trips() {
        let query = SearchQuery {
            search_term: "matrix".to_owned(),
            limit: Some(20),
            media_types: vec![MediaType::Video],
            ..SearchQuery::default()
        };
        let json = serde_json::to_value(&query).unwrap();
        assert_eq!(json["SearchTerm"], "matrix");
        assert_eq!(json["Limit"], 20);
        assert_eq!(json["MediaTypes"], serde_json::json!(["Video"]));
        let back: SearchQuery = serde_json::from_value(json).unwrap();
        assert_eq!(query, back);
    }
}
