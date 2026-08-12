//! Port of `Emby.Naming.Video.VideoInfo`.

use ferrofin_model::entities::ExtraType;

use crate::video::VideoFileInfo;

/// Represents a complete video, including all parts and subtitles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoInfo {
    /// The name.
    pub name: Option<String>,
    /// The year.
    pub year: Option<i32>,
    /// The files.
    pub files: Vec<VideoFileInfo>,
    /// The alternate versions. Each alternate may itself span multiple files.
    pub alternate_versions: Vec<VideoInfo>,
    /// The extra type.
    pub extra_type: Option<ExtraType>,
}

impl VideoInfo {
    /// Creates a new [`VideoInfo`] with the given name and empty file lists.
    #[must_use]
    pub fn new(name: Option<String>) -> Self {
        Self {
            name,
            year: None,
            files: Vec::new(),
            alternate_versions: Vec::new(),
            extra_type: None,
        }
    }
}
