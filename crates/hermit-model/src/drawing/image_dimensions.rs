//! `ImageDimensions` — port of `MediaBrowser.Model.Drawing.ImageDimensions`.

use std::fmt;

/// Struct `ImageDimensions` — a width/height pair, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ImageDimensions {
    /// Gets the width.
    pub width: i32,
    /// Gets the height.
    pub height: i32,
}

impl ImageDimensions {
    /// Creates a new [`ImageDimensions`] from a width and height.
    #[must_use]
    pub fn new(width: i32, height: i32) -> Self {
        Self { width, height }
    }
}

impl fmt::Display for ImageDimensions {
    /// Formats as `{width}-{height}` (invariant culture in the C# original).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.width, self.height)
    }
}
