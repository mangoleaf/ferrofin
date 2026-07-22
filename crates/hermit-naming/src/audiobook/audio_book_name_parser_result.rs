//! Port of `Emby.Naming.AudioBook.AudioBookNameParserResult`.

/// Result of audiobook name and year parsing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioBookNameParserResult {
    /// The name of the audiobook.
    pub name: Option<String>,
    /// Optional year of release.
    pub year: Option<i32>,
}
