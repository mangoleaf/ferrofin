//! Port of `ReadOnlyListExtension.cs` — `IndexOf`/`FindIndex`/`FirstOrDefault`
//! over read-only slices, mapping to std slice/iterator methods.

/// Finds the index of the first element equal to `value`, or `None` if absent.
///
/// C# returned `-1` when not found; the idiomatic Rust equivalent is `None`.
pub fn index_of<T: PartialEq>(source: &[T], value: &T) -> Option<usize> {
    source.iter().position(|item| item == value)
}

/// Finds the index of the first element satisfying `matches`, or `None`.
pub fn find_index<T, F: FnMut(&T) -> bool>(source: &[T], matches: F) -> Option<usize> {
    source.iter().position(matches)
}

/// Gets the first element of the slice, or `None` if it is empty.
pub fn first_or_default<T>(source: &[T]) -> Option<&T> {
    source.first()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_of_found_and_missing() {
        let src = [10, 20, 30];
        assert_eq!(Some(1), index_of(&src, &20));
        assert_eq!(None, index_of(&src, &99));
    }

    #[test]
    fn find_index_predicate() {
        let src = [1, 2, 3, 4];
        assert_eq!(Some(2), find_index(&src, |&x| x > 2));
        assert_eq!(None, find_index(&src, |&x| x > 10));
    }

    #[test]
    fn first_or_default_semantics() {
        let src = [7, 8];
        assert_eq!(Some(&7), first_or_default(&src));
        let empty: [i32; 0] = [];
        assert_eq!(None, first_or_default(&empty));
    }
}
