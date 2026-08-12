//! Port of `Emby.Naming.TV.SeriesInfo`.

/// Holder object for series information.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SeriesInfo {
    /// The path.
    pub path: String,
    /// The name of the series.
    pub name: Option<String>,
    /// The year of the series.
    pub year: Option<i32>,
}

impl SeriesInfo {
    /// Creates a new [`SeriesInfo`] for the given path.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            name: None,
            year: None,
        }
    }
}
