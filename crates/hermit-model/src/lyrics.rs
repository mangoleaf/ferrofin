//! Port of `MediaBrowser.Model.Lyrics`.
//!
//! `LyricResponse` (carries a raw `Stream`) and `UploadLyricDto` (carries an
//! `IFormFile`) are server-side transport types, not wire DTOs, so they are
//! dropped from this port.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// `LyricMetadata` model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LyricMetadata {
    /// Gets or sets the song artist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,

    /// Gets or sets the album this song is on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,

    /// Gets or sets the title of the song.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,

    /// Gets or sets the author of the lyric data.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author: Option<String>,

    /// Gets or sets the length of the song in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub length: Option<i64>,

    /// Gets or sets who the LRC file was created by.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,

    /// Gets or sets the lyric offset compared to audio in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<i64>,

    /// Gets or sets the software used to create the LRC file.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,

    /// Gets or sets the version of the creator used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Gets or sets a value indicating whether this lyric is synced.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_synced: Option<bool>,
}

/// `LyricLineCue` model, holds information about the timing of words within a
/// [`LyricLine`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LyricLineCue {
    /// Gets the start character index of the cue.
    pub position: i32,

    /// Gets the end character index of the cue.
    pub end_position: i32,

    /// Gets the timestamp the lyric is synced to in ticks.
    pub start: i64,

    /// Gets the end timestamp the lyric is synced to in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<i64>,
}

/// Lyric line model.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LyricLine {
    /// Gets the text of this lyric line.
    pub text: String,

    /// Gets the start time in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start: Option<i64>,

    /// Gets the time-aligned cues for the song's lyrics.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cues: Option<Vec<LyricLineCue>>,
}

/// Lyric DTO model (metadata plus the individual lyric lines).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LyricDto {
    /// Gets or sets metadata for the lyrics.
    pub metadata: LyricMetadata,

    /// Gets or sets a collection of individual lyric lines.
    pub lyrics: Vec<LyricLine>,
}

/// The information for a raw lyrics file before parsing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LyricFile {
    /// Gets or sets the name of the lyrics file. This must include the file
    /// extension.
    pub name: String,

    /// Gets or sets the contents of the file.
    pub content: String,
}

/// Lyric search request.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct LyricSearchRequest {
    /// Gets or sets the media path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_path: Option<String>,

    /// Gets or sets the album artist names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_artists_names: Option<Vec<String>>,

    /// Gets or sets the artist names.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_names: Option<Vec<String>>,

    /// Gets or sets the album name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album_name: Option<String>,

    /// Gets or sets the song name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub song_name: Option<String>,

    /// Gets or sets the track duration in ticks.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<i64>,

    /// Gets or sets the provider ids.
    pub provider_ids: HashMap<String, String>,

    /// Gets or sets a value indicating whether to search all providers.
    pub search_all_providers: bool,

    /// Gets or sets the list of disabled lyric fetcher names.
    pub disabled_lyric_fetchers: Vec<String>,

    /// Gets or sets the order of lyric fetchers.
    pub lyric_fetcher_order: Vec<String>,

    /// Gets or sets a value indicating whether this request is automated.
    pub is_automated: bool,
}

/// The remote lyric info DTO.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "PascalCase")]
pub struct RemoteLyricInfoDto {
    /// Gets or sets the id for the lyric.
    pub id: String,

    /// Gets the provider name.
    pub provider_name: String,

    /// Gets the lyrics.
    pub lyrics: LyricDto,
}
