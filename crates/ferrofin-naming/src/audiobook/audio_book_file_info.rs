//! Port of `Emby.Naming.AudioBook.AudioBookFileInfo`.

use std::cmp::Ordering;

/// Represents a single audiobook file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioBookFileInfo {
    /// The path.
    pub path: String,
    /// The container (file type).
    pub container: String,
    /// The part number this file represents.
    pub part_number: Option<i32>,
    /// The chapter number this file represents.
    pub chapter_number: Option<i32>,
}

impl AudioBookFileInfo {
    /// Creates a new [`AudioBookFileInfo`].
    #[must_use]
    pub fn new(
        path: impl Into<String>,
        container: impl Into<String>,
        part_number: Option<i32>,
        chapter_number: Option<i32>,
    ) -> Self {
        Self {
            path: path.into(),
            container: container.into(),
            part_number,
            chapter_number,
        }
    }
}

impl Ord for AudioBookFileInfo {
    fn cmp(&self, other: &Self) -> Ordering {
        // Chapter, then part (both `None` sorts before `Some`, matching
        // .NET `Nullable.Compare`), then ordinal path.
        nullable_compare(self.chapter_number, other.chapter_number)
            .then_with(|| nullable_compare(self.part_number, other.part_number))
            .then_with(|| self.path.cmp(&other.path))
    }
}

impl PartialOrd for AudioBookFileInfo {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Mirrors `Nullable.Compare`: `None` is less than any `Some`.
fn nullable_compare(a: Option<i32>, b: Option<i32>) -> Ordering {
    match (a, b) {
        (None, None) => Ordering::Equal,
        (None, Some(_)) => Ordering::Less,
        (Some(_), None) => Ordering::Greater,
        (Some(x), Some(y)) => x.cmp(&y),
    }
}
