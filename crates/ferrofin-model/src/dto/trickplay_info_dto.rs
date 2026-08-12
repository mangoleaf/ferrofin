//! `TrickplayInfoDto` — port of `MediaBrowser.Model.Dto.TrickplayInfoDto`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// The trickplay API model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct TrickplayInfoDto {
    /// Gets the width of an individual thumbnail.
    pub width: i32,
    /// Gets the height of an individual thumbnail.
    pub height: i32,
    /// Gets the amount of thumbnails per row.
    pub tile_width: i32,
    /// Gets the amount of thumbnails per column.
    pub tile_height: i32,
    /// Gets the total amount of non-black thumbnails.
    pub thumbnail_count: i32,
    /// Gets the interval in milliseconds between each trickplay thumbnail.
    pub interval: i32,
    /// Gets the peak bandwidth usage in bits per second.
    pub bandwidth: i32,
}
