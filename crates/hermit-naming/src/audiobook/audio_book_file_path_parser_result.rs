//! Port of `Emby.Naming.AudioBook.AudioBookFilePathParserResult`.

/// Result of audiobook part/chapter extraction from a filename.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AudioBookFilePathParserResult {
    /// Optional part number extracted from the filename.
    pub part_number: Option<i32>,
    /// Optional chapter number extracted from the filename.
    pub chapter_number: Option<i32>,
}
