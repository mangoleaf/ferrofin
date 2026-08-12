//! Port of `SplitStringExtensions.cs` — an allocation-free single-`char` split.
//!
//! The C# `ref struct Enumerator` maps cleanly onto a borrowing iterator over
//! string slices; a thin wrapper preserves parity with the original API. Unlike
//! `str::split`, the C# enumerator yields nothing for an empty input (it returns
//! `false` immediately when the span is empty).

/// Iterator yielding the substrings of a string separated by a single `char`.
///
/// Mirrors the semantics of the C# `SplitStringExtensions.Enumerator`: an empty
/// input yields no items; otherwise it yields each segment, including a trailing
/// empty segment when the input ends with the separator.
pub struct SplitEnumerator<'a> {
    remainder: Option<&'a str>,
    separator: char,
}

impl<'a> Iterator for SplitEnumerator<'a> {
    type Item = &'a str;

    fn next(&mut self) -> Option<&'a str> {
        let current = self.remainder?;
        match current.find(self.separator) {
            None => {
                self.remainder = None;
                Some(current)
            }
            Some(index) => {
                let (head, tail) = current.split_at(index);
                self.remainder = Some(&tail[self.separator.len_utf8()..]);
                Some(head)
            }
        }
    }
}

/// Creates a new split enumerator over `str` on `separator`.
#[must_use]
pub fn span_split(str: &str, separator: char) -> SplitEnumerator<'_> {
    let remainder = if str.is_empty() { None } else { Some(str) };
    SplitEnumerator {
        remainder,
        separator,
    }
}

/// Alias matching the C# `Split` overload for a span.
#[must_use]
pub fn split(str: &str, separator: char) -> SplitEnumerator<'_> {
    span_split(str, separator)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_yields_nothing() {
        let parts: Vec<&str> = span_split("", ',').collect();
        assert!(parts.is_empty());
    }

    #[test]
    fn splits_on_separator() {
        let parts: Vec<&str> = span_split("a,b,c", ',').collect();
        assert_eq!(vec!["a", "b", "c"], parts);
    }

    #[test]
    fn no_separator_yields_whole() {
        let parts: Vec<&str> = span_split("abc", ',').collect();
        assert_eq!(vec!["abc"], parts);
    }

    #[test]
    fn trailing_separator_yields_empty_segment() {
        let parts: Vec<&str> = span_split("a,", ',').collect();
        assert_eq!(vec!["a", ""], parts);
    }
}
