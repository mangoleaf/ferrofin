//! The placeholder lists of the `IN (…)` queries.

use std::fmt::Write as _;
use std::ops::RangeInclusive;

/// The numbered SQL parameters `?first,…,?last`, comma-separated: what goes
/// between the parentheses of an `IN (…)` over a chunk of ids (`1..=chunk.len()`,
/// or `2..=chunk.len() + 1` when `?1` is another parameter). An empty range is an
/// empty string.
#[must_use]
pub fn numbered_placeholders(numbers: RangeInclusive<usize>) -> String {
    let (first, last) = (*numbers.start(), *numbers.end());
    let widest = last.checked_ilog10().map_or(1, |log| log as usize + 1);
    // `?`, the digits and `,`: the commas leave room for the one not written.
    let mut sql = String::with_capacity((last + 1).saturating_sub(first) * (widest + 2));
    for number in numbers {
        if !sql.is_empty() {
            sql.push(',');
        }
        // Writing to a String cannot fail.
        let _ = write!(sql, "?{number}");
    }
    sql
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The SQL text, and with it each statement's cached plan, stays what the
    /// `format!` + `join` idiom this replaced wrote.
    #[rstest::rstest]
    #[case(1..=1)]
    #[case(1..=3)]
    #[case(2..=4)]
    #[case(1..=9)]
    #[case(1..=10)]
    #[case(9..=11)]
    #[case(99..=101)]
    #[case(1..=500)]
    #[case(2..=501)]
    #[case(501..=504)]
    fn writes_the_text_the_format_and_join_idiom_wrote(#[case] numbers: RangeInclusive<usize>) {
        let idiom = numbers
            .clone()
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(",");
        assert_eq!(numbered_placeholders(numbers), idiom);
    }

    #[test]
    fn a_list_is_written_with_no_spaces() {
        assert_eq!(numbered_placeholders(2..=4), "?2,?3,?4");
    }

    /// `n + 1..=n + kinds` with no kinds, and an empty chunk.
    #[test]
    fn an_empty_range_is_an_empty_list() {
        let (first, count) = (3, 0);
        assert_eq!(numbered_placeholders(first..=first + count - 1), "");
        assert_eq!(numbered_placeholders(1..=count), "");
    }

    /// The buffer is sized up front, so a list never reallocates as it grows.
    #[rstest::rstest]
    #[case(1..=1)]
    #[case(1..=500)]
    #[case(2..=501)]
    #[case(995..=1004)]
    fn the_buffer_is_sized_up_front(#[case] numbers: RangeInclusive<usize>) {
        let reserved = numbers.clone().count() * (numbers.end().to_string().len() + 2);
        let sql = numbered_placeholders(numbers);
        assert!(sql.len() <= reserved, "{sql}");
        assert!(sql.capacity() >= reserved, "{sql}");
    }
}
