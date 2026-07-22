//! Request type for [`crate::DynamicHlsPlaylistGenerator::create_main_playlist`].
//!
//! Port of `Jellyfin.MediaEncoding.Hls.Playlist.CreateMainPlaylistRequest`.

use uuid::Uuid;

/// Request for creating the main HLS playlist containing the primary video or
/// audio stream.
///
/// Mirrors the C# `CreateMainPlaylistRequest` constructor field-for-field. The
/// `Guid?` media source id ports to `Option<Uuid>`; all other fields are direct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateMainPlaylistRequest {
    /// The media source id.
    pub media_source_id: Option<Uuid>,

    /// The absolute file path to the file.
    pub file_path: String,

    /// The desired segment length in milliseconds.
    pub desired_segment_length_ms: i32,

    /// The total duration of the file in ticks.
    pub total_runtime_ticks: i64,

    /// The desired segment container e.g. `"ts"`.
    pub segment_container: String,

    /// The URI prefix for the relative URL in the playlist.
    pub endpoint_prefix: String,

    /// The desired query string to append (must start with `?`).
    pub query_string: String,

    /// Whether the video is being remuxed.
    pub is_remuxing_video: bool,
}

impl CreateMainPlaylistRequest {
    /// Initializes a new [`CreateMainPlaylistRequest`].
    ///
    /// # Arguments
    ///
    /// * `media_source_id` - The media source id.
    /// * `file_path` - The absolute file path to the file.
    /// * `desired_segment_length_ms` - The desired segment length in milliseconds.
    /// * `total_runtime_ticks` - The total duration of the file in ticks.
    /// * `segment_container` - The desired segment container e.g. `"ts"`.
    /// * `endpoint_prefix` - The URI prefix for the relative URL in the playlist.
    /// * `query_string` - The desired query string to append (must start with `?`).
    /// * `is_remuxing_video` - Whether the video is being remuxed.
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        media_source_id: Option<Uuid>,
        file_path: impl Into<String>,
        desired_segment_length_ms: i32,
        total_runtime_ticks: i64,
        segment_container: impl Into<String>,
        endpoint_prefix: impl Into<String>,
        query_string: impl Into<String>,
        is_remuxing_video: bool,
    ) -> Self {
        Self {
            media_source_id,
            file_path: file_path.into(),
            desired_segment_length_ms,
            total_runtime_ticks,
            segment_container: segment_container.into(),
            endpoint_prefix: endpoint_prefix.into(),
            query_string: query_string.into(),
            is_remuxing_video,
        }
    }
}
