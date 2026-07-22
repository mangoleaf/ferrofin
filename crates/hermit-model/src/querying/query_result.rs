//! `QueryResult<T>`, `ThemeMediaResult`, and `AllThemeMediaResult` — port of
//! the matching types in `MediaBrowser.Model.Querying`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use uuid::Uuid;

use crate::dto::BaseItemDto;

/// A paged query-result container.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct QueryResult<T> {
    /// Gets or sets the items.
    pub items: Vec<T>,

    /// Gets or sets the total number of records available.
    pub total_record_count: i32,

    /// Gets or sets the index of the first record in `Items`.
    pub start_index: i32,
}

impl<T> Default for QueryResult<T> {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            total_record_count: 0,
            start_index: 0,
        }
    }
}

impl<T> QueryResult<T> {
    /// Initializes a new [`QueryResult`] from a list of items, setting
    /// `total_record_count` to the item count and `start_index` to zero.
    #[must_use]
    pub fn from_items(items: Vec<T>) -> Self {
        let total_record_count = i32::try_from(items.len()).unwrap_or(i32::MAX);
        Self {
            items,
            total_record_count,
            start_index: 0,
        }
    }

    /// Initializes a new [`QueryResult`] with an explicit start index and total
    /// record count.
    #[must_use]
    pub fn new(start_index: Option<i32>, total_record_count: Option<i32>, items: Vec<T>) -> Self {
        let count = i32::try_from(items.len()).unwrap_or(i32::MAX);
        Self {
            start_index: start_index.unwrap_or(0),
            total_record_count: total_record_count.unwrap_or(count),
            items,
        }
    }
}

/// A [`QueryResult`] of theme media, tagged with the owning item's id.
///
/// Upstream this extends `QueryResult<BaseItemDto>`; the base result is exposed
/// here as a flattened `result` field since Rust has no struct inheritance.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ThemeMediaResult {
    /// The underlying query result.
    #[serde(flatten)]
    pub result: QueryResult<BaseItemDto>,

    /// Gets or sets the owner id.
    #[schema(value_type = String, format = "uuid")]
    pub owner_id: Uuid,
}

/// The combined theme-media results (videos, songs, and soundtracks).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct AllThemeMediaResult {
    /// Gets or sets the theme videos result.
    pub theme_videos_result: ThemeMediaResult,

    /// Gets or sets the theme songs result.
    pub theme_songs_result: ThemeMediaResult,

    /// Gets or sets the soundtrack songs result.
    pub soundtrack_songs_result: ThemeMediaResult,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_empty() {
        let value = QueryResult::<i32>::default();
        assert!(value.items.is_empty());
        assert_eq!(value.total_record_count, 0);
        assert_eq!(value.start_index, 0);
    }

    #[test]
    fn from_items_sets_total_to_len() {
        let value = QueryResult::from_items(vec![1, 2, 3]);
        assert_eq!(value.items, vec![1, 2, 3]);
        assert_eq!(value.total_record_count, 3);
        assert_eq!(value.start_index, 0);
    }

    #[test]
    fn new_with_explicit_paging() {
        let value = QueryResult::new(Some(10), Some(100), vec!["a".to_owned()]);
        assert_eq!(value.start_index, 10);
        assert_eq!(value.total_record_count, 100);
        assert_eq!(value.items.len(), 1);
    }

    #[test]
    fn new_defaults_total_to_item_count() {
        let value = QueryResult::new(None, None, vec![1, 2]);
        assert_eq!(value.start_index, 0);
        assert_eq!(value.total_record_count, 2);
    }

    #[test]
    fn round_trips_through_json() {
        let value = QueryResult::new(Some(5), Some(50), vec!["x".to_owned(), "y".to_owned()]);
        let json = serde_json::to_value(&value).unwrap();
        assert_eq!(json["Items"], serde_json::json!(["x", "y"]));
        assert_eq!(json["TotalRecordCount"], 50);
        assert_eq!(json["StartIndex"], 5);
        let back: QueryResult<String> = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn theme_media_result_flattens_base_and_round_trips() {
        let value = ThemeMediaResult {
            result: QueryResult::from_items(Vec::new()),
            owner_id: Uuid::from_u128(0xABCD),
        };
        let json = serde_json::to_value(&value).unwrap();
        // Flattened base fields appear at the top level, not nested.
        assert_eq!(json["TotalRecordCount"], 0);
        assert_eq!(json["OwnerId"], value.owner_id.to_string());
        let back: ThemeMediaResult = serde_json::from_value(json).unwrap();
        assert_eq!(value, back);
    }

    #[test]
    fn all_theme_media_result_round_trips() {
        let value = AllThemeMediaResult::default();
        let back: AllThemeMediaResult =
            serde_json::from_str(&serde_json::to_string(&value).unwrap()).unwrap();
        assert_eq!(value, back);
    }
}
