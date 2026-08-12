//! Keyframe information for a specific file.
//!
//! Port of `Jellyfin.MediaEncoding.Keyframes.KeyframeData` (`KeyframeData.cs`).

use serde::{Deserialize, Serialize};

/// Keyframe information for a specific file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub struct KeyframeData {
    /// Gets the total duration of the stream in ticks.
    pub total_duration: i64,

    /// Gets the keyframes in ticks.
    pub keyframe_ticks: Vec<i64>,
}

impl KeyframeData {
    /// Initializes a new instance of the [`KeyframeData`] struct.
    ///
    /// # Arguments
    ///
    /// * `total_duration` - The total duration of the video stream in ticks.
    /// * `keyframe_ticks` - The video keyframes in ticks.
    #[must_use]
    pub fn new(total_duration: i64, keyframe_ticks: Vec<i64>) -> Self {
        Self {
            total_duration,
            keyframe_ticks,
        }
    }
}
