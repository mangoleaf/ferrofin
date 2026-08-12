//! `ItemCounts` — port of `MediaBrowser.Model.Dto.ItemCounts`.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Class holding a summary of item counts in a library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct ItemCounts {
    /// Gets or sets the movie count.
    pub movie_count: i32,
    /// Gets or sets the series count.
    pub series_count: i32,
    /// Gets or sets the episode count.
    pub episode_count: i32,
    /// Gets or sets the artist count.
    pub artist_count: i32,
    /// Gets or sets the program count.
    pub program_count: i32,
    /// Gets or sets the trailer count.
    pub trailer_count: i32,
    /// Gets or sets the song count.
    pub song_count: i32,
    /// Gets or sets the album count.
    pub album_count: i32,
    /// Gets or sets the music video count.
    pub music_video_count: i32,
    /// Gets or sets the box set count.
    pub box_set_count: i32,
    /// Gets or sets the book count.
    pub book_count: i32,
    /// Gets or sets the item count.
    pub item_count: i32,
}

impl ItemCounts {
    /// Adds all counts (excluding `ItemCount`).
    #[must_use]
    pub fn total_item_count(&self) -> i32 {
        self.movie_count
            + self.series_count
            + self.episode_count
            + self.artist_count
            + self.program_count
            + self.trailer_count
            + self.song_count
            + self.album_count
            + self.music_video_count
            + self.box_set_count
            + self.book_count
    }
}
