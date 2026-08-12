//! Port of `Emby.Naming.Video.VideoFileInfo`.

use std::fmt;

use ferrofin_model::entities::ExtraType;

use crate::path;
use crate::video::ExtraRule;

/// Represents a single video file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFileInfo {
    /// The path.
    pub path: String,
    /// The container.
    pub container: Option<String>,
    /// The name.
    pub name: String,
    /// The year.
    pub year: Option<i32>,
    /// The type of the extra (trailer, theme song, behind the scenes, ...).
    pub extra_type: Option<ExtraType>,
    /// The extra rule.
    pub extra_rule: Option<ExtraRule>,
    /// The 3D format.
    pub format_3d: Option<String>,
    /// Whether the file is 3D.
    pub is_3d: bool,
    /// Whether the file is a stub.
    pub is_stub: bool,
    /// The stub type.
    pub stub_type: Option<String>,
    /// Whether the file is a directory.
    pub is_directory: bool,
}

impl VideoFileInfo {
    /// Creates a new [`VideoFileInfo`] with the given `name` and `path`.
    ///
    /// Remaining fields default to their C# defaults; use the struct-update
    /// syntax to set the rest.
    #[must_use]
    pub fn new(name: impl Into<String>, path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            container: None,
            name: name.into(),
            year: None,
            extra_type: None,
            extra_rule: None,
            format_3d: None,
            is_3d: false,
            is_stub: false,
            stub_type: None,
            is_directory: false,
        }
    }

    /// Returns the file name without extension (or the folder name when this is
    /// a directory).
    #[must_use]
    pub fn file_name_without_extension(&self) -> &str {
        if self.is_directory {
            path::file_name(&self.path)
        } else {
            path::file_name_without_extension(&self.path)
        }
    }
}

impl fmt::Display for VideoFileInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "VideoFileInfo(Name: '{}')", self.name)
    }
}
