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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_width_and_height() {
        let dims = ImageDimensions::new(1920, 1080);
        assert_eq!(dims.width, 1920);
        assert_eq!(dims.height, 1080);
    }

    #[test]
    fn default_is_zero_by_zero() {
        let dims = ImageDimensions::default();
        assert_eq!(dims, ImageDimensions::new(0, 0));
        assert_eq!(dims.width, 0);
        assert_eq!(dims.height, 0);
    }

    #[test]
    fn display_uses_dash_separator() {
        assert_eq!(ImageDimensions::new(1920, 1080).to_string(), "1920-1080");
        assert_eq!(ImageDimensions::new(0, 0).to_string(), "0-0");
        assert_eq!(ImageDimensions::new(-1, 2).to_string(), "-1-2");
    }

    #[test]
    fn equality_and_copy() {
        let a = ImageDimensions::new(4, 8);
        let b = a;
        assert_eq!(a, b);
        assert_ne!(a, ImageDimensions::new(8, 4));
    }
}
