//! Port of `Emby.Naming.ExternalFiles.ExternalPathParserResult`.

/// Information parsed about an external file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExternalPathParserResult {
    /// The path.
    pub path: String,
    /// The language.
    pub language: Option<String>,
    /// The title.
    pub title: Option<String>,
    /// Whether this instance is default.
    pub is_default: bool,
    /// Whether this instance is forced.
    pub is_forced: bool,
    /// Whether this instance is for the hearing impaired.
    pub is_hearing_impaired: bool,
}

impl ExternalPathParserResult {
    /// Creates a new [`ExternalPathParserResult`] for the given path.
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            language: None,
            title: None,
            is_default: false,
            is_forced: false,
            is_hearing_impaired: false,
        }
    }
}
