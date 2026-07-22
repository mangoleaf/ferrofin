//! Port of `EnumerableExtensions.cs` — the portable members `Contains` (with a
//! string-comparison mode) and `SingleItemAsEnumerable`.
//!
//! `GetUniqueFlags` relied on C# `[Flags]` enum reflection and has no Rust
//! analog; consumers should iterate a `bitflags!` type directly, so it is
//! dropped here per the inventory.

/// The subset of `System.StringComparison` values needed by `contains`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StringComparison {
    /// Case-sensitive ordinal comparison.
    Ordinal,
    /// Case-insensitive ordinal comparison (ASCII-case + Unicode simple folding).
    OrdinalIgnoreCase,
}

/// Determines whether `value` is contained in `source` under the given
/// string-comparison mode.
pub fn contains<'a, I>(source: I, value: &str, comparison: StringComparison) -> bool
where
    I: IntoIterator<Item = &'a str>,
{
    source.into_iter().any(|element| match comparison {
        StringComparison::Ordinal => element == value,
        StringComparison::OrdinalIgnoreCase => element.eq_ignore_ascii_case(value),
    })
}

/// Gets an iterator yielding a single item.
pub fn single_item_as_enumerable<T>(item: T) -> std::iter::Once<T> {
    std::iter::once(item)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_ordinal() {
        let src = ["Alpha", "Beta"];
        assert!(contains(
            src.iter().copied(),
            "Beta",
            StringComparison::Ordinal
        ));
        assert!(!contains(
            src.iter().copied(),
            "beta",
            StringComparison::Ordinal
        ));
    }

    #[test]
    fn contains_ordinal_ignore_case() {
        let src = ["Alpha", "Beta"];
        assert!(contains(
            src.iter().copied(),
            "beta",
            StringComparison::OrdinalIgnoreCase
        ));
    }

    #[test]
    fn single_item() {
        let items: Vec<i32> = single_item_as_enumerable(42).collect();
        assert_eq!(vec![42], items);
    }
}
