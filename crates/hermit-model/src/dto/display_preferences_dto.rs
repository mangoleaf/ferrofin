//! `DisplayPreferencesDto` — port of
//! `MediaBrowser.Model.Dto.DisplayPreferencesDto`.
//!
//! The `ScrollDirection`/`SortOrder` enums live upstream in
//! `Jellyfin.Database.Implementations.Enums`; they are defined here as they are
//! not otherwise ported and only referenced by this DTO.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// An enum representing the axis that should be scrolled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum ScrollDirection {
    /// Horizontal scrolling direction.
    #[default]
    Horizontal,
    /// Vertical scrolling direction.
    Vertical,
}

/// An enum representing the sorting order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub enum SortOrder {
    /// Sort in increasing order.
    #[default]
    Ascending,
    /// Sort in decreasing order.
    Descending,
}

/// Display preferences for any item that supports them (usually folders).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct DisplayPreferencesDto {
    /// Gets or sets the user id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,

    /// Gets or sets the type of the view.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub view_type: Option<String>,

    /// Gets or sets the sort by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sort_by: Option<String>,

    /// Gets or sets the index by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_by: Option<String>,

    /// Gets or sets a value indicating whether to remember indexing.
    pub remember_indexing: bool,

    /// Gets or sets the height of the primary image.
    pub primary_image_height: i32,

    /// Gets or sets the width of the primary image.
    pub primary_image_width: i32,

    /// Gets or sets the custom prefs.
    pub custom_prefs: HashMap<String, Option<String>>,

    /// Gets or sets the scroll direction.
    pub scroll_direction: ScrollDirection,

    /// Gets or sets a value indicating whether to show backdrops on this item.
    pub show_backdrop: bool,

    /// Gets or sets a value indicating whether to remember sorting.
    pub remember_sorting: bool,

    /// Gets or sets the sort order.
    pub sort_order: SortOrder,

    /// Gets or sets a value indicating whether to show the sidebar.
    pub show_sidebar: bool,

    /// Gets or sets the client.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client: Option<String>,
}

impl Default for DisplayPreferencesDto {
    fn default() -> Self {
        Self {
            id: None,
            view_type: None,
            sort_by: None,
            index_by: None,
            remember_indexing: false,
            primary_image_height: 250,
            primary_image_width: 250,
            custom_prefs: HashMap::new(),
            scroll_direction: ScrollDirection::default(),
            show_backdrop: true,
            remember_sorting: false,
            sort_order: SortOrder::default(),
            show_sidebar: false,
            client: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_dimensions_and_flags() {
        let prefs = DisplayPreferencesDto::default();
        assert_eq!(prefs.primary_image_height, 250);
        assert_eq!(prefs.primary_image_width, 250);
        assert!(prefs.show_backdrop);
        assert_eq!(prefs.scroll_direction, ScrollDirection::Horizontal);
        assert_eq!(prefs.sort_order, SortOrder::Ascending);
    }

    #[test]
    fn round_trips_through_json() {
        let mut custom = HashMap::new();
        custom.insert("theme".to_owned(), Some("dark".to_owned()));
        custom.insert("empty".to_owned(), None);
        let prefs = DisplayPreferencesDto {
            id: Some("user1".to_owned()),
            view_type: Some("Poster".to_owned()),
            custom_prefs: custom,
            scroll_direction: ScrollDirection::Vertical,
            sort_order: SortOrder::Descending,
            ..DisplayPreferencesDto::default()
        };
        let json = serde_json::to_string(&prefs).unwrap();
        let back: DisplayPreferencesDto = serde_json::from_str(&json).unwrap();
        assert_eq!(prefs, back);
    }

    #[test]
    fn uses_contract_field_names() {
        let json = serde_json::to_value(DisplayPreferencesDto::default()).unwrap();
        assert_eq!(json["PrimaryImageHeight"], 250);
        assert_eq!(json["ScrollDirection"], "Horizontal");
        assert_eq!(json["SortOrder"], "Ascending");
        assert_eq!(json["ShowBackdrop"], true);
    }

    #[test]
    fn scroll_direction_and_sort_order_round_trip() {
        for dir in [ScrollDirection::Horizontal, ScrollDirection::Vertical] {
            let back: ScrollDirection =
                serde_json::from_str(&serde_json::to_string(&dir).unwrap()).unwrap();
            assert_eq!(dir, back);
        }
        for order in [SortOrder::Ascending, SortOrder::Descending] {
            let back: SortOrder =
                serde_json::from_str(&serde_json::to_string(&order).unwrap()).unwrap();
            assert_eq!(order, back);
        }
    }
}
