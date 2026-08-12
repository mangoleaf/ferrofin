//! Port of `Emby.Naming.Video.CleanDateTimeResult`.

/// Holder structure for a cleaned name and optional year.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CleanDateTimeResult {
    /// The cleaned name.
    pub name: String,
    /// The parsed year of release, if any.
    pub year: Option<i32>,
}

impl CleanDateTimeResult {
    /// Creates a new [`CleanDateTimeResult`].
    #[must_use]
    pub fn new(name: impl Into<String>, year: Option<i32>) -> Self {
        Self {
            name: name.into(),
            year,
        }
    }
}
