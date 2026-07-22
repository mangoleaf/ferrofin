//! Port of `Emby.Naming.Video.Format3DResult`.

/// Helper object returned from [`crate::video::Format3DParser`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Format3DResult {
    /// Whether the parsed string contains 3D tokens.
    pub is_3d: bool,
    /// The 3D format; `None` when [`Self::is_3d`] is `false`.
    pub format_3d: Option<String>,
}

impl Format3DResult {
    /// Creates a new [`Format3DResult`].
    #[must_use]
    pub fn new(is_3d: bool, format_3d: Option<String>) -> Self {
        Self { is_3d, format_3d }
    }
}
