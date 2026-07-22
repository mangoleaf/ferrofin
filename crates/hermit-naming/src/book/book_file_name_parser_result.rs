//! Port of `Emby.Naming.Book.BookFileNameParserResult`.

/// Metadata parsed from a book filename.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BookFileNameParserResult {
    /// The name of the book.
    pub name: Option<String>,
    /// The book index.
    pub index: Option<i32>,
    /// The parent index number.
    pub parent_index: Option<i32>,
    /// The publication year.
    pub year: Option<i32>,
    /// The series name.
    pub series_name: Option<String>,
}
