//! The placeholder lists of the `IN (…)` queries and multi-row inserts.

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

/// The `VALUES` list of a multi-row insert: `rows` groups of `columns`
/// anonymous parameters, `(?, ?), (?, ?)`.
#[must_use]
pub fn values_rows(rows: usize, columns: usize) -> String {
    let row = format!("({})", vec!["?"; columns].join(", "));
    vec![row; rows].join(", ")
}

/// The binds of the `IN (…)` reads over `items`, a chunk at a time: at most
/// the largest power of two within [`BATCH_BIND_CHUNK`](crate::BATCH_BIND_CHUNK)
/// per chunk, each padded to a power-of-two length by repeating its last item.
///
/// A repeated value changes no `IN`, so up to that size the read stays one
/// statement, and its text is one of a few lengths in the prepared-statement
/// cache (gl-1071) whatever the count.
pub fn padded_in_chunks<T>(items: &[T]) -> impl Iterator<Item = Vec<&T>> {
    let cap = 1_usize << crate::BATCH_BIND_CHUNK.ilog2();
    items.chunks(cap).map(|chunk| {
        let pad = chunk.len().next_power_of_two() - chunk.len();
        chunk
            .iter()
            .chain(std::iter::repeat_n(chunk.last(), pad).flatten())
            .collect()
    })
}

/// `items` in runs whose lengths are powers of two, largest first, none
/// longer than `max_len` (at least 1).
///
/// A multi-row statement's text depends on its row count, and each text is
/// its own entry in the connection's prepared-statement cache (gl-1071). So a
/// statement written over these runs has at most `log2(max_len) + 1` texts,
/// whatever the counts, instead of one per count.
pub fn power_of_two_chunks<T>(items: &[T], max_len: usize) -> impl Iterator<Item = &[T]> {
    let cap = 1_usize << max_len.max(1).ilog2();
    let mut rest = items;
    std::iter::from_fn(move || {
        let len = rest
            .len()
            .checked_ilog2()
            .map(|log| (1_usize << log).min(cap))?;
        let (run, tail) = rest.split_at(len);
        rest = tail;
        Some(run)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_rows_writes_one_group_per_row() {
        assert_eq!(values_rows(2, 3), "(?, ?, ?), (?, ?, ?)");
        assert_eq!(values_rows(1, 1), "(?)");
        assert_eq!(values_rows(0, 4), "");
    }

    /// Each chunk is the items in order, then the last repeated up to a power
    /// of two; no chunk is longer than 256.
    #[rstest::rstest]
    #[case(0, &[])]
    #[case(1, &[1])]
    #[case(3, &[4])]
    #[case(4, &[4])]
    #[case(5, &[8])]
    #[case(256, &[256])]
    #[case(300, &[256, 64])]
    fn padded_in_chunks_repeat_the_last_item_up_to_a_power_of_two(
        #[case] count: usize,
        #[case] lens: &[usize],
    ) {
        let items: Vec<usize> = (0..count).collect();
        let chunks: Vec<Vec<&usize>> = padded_in_chunks(&items).collect();
        assert_eq!(chunks.iter().map(Vec::len).collect::<Vec<_>>(), lens);
        let mut rest = items.as_slice();
        for chunk in &chunks {
            let real = rest.len().min(256);
            let (head, tail) = rest.split_at(real);
            assert!(chunk[..real].iter().copied().eq(head.iter()));
            assert!(chunk[real..].iter().all(|item| Some(*item) == head.last()));
            rest = tail;
        }
        assert_eq!(rest, [0_usize; 0]);
    }

    /// Every item once, in order, in power-of-two runs no longer than the cap.
    #[rstest::rstest]
    #[case(0, 8, &[])]
    #[case(1, 8, &[1])]
    #[case(7, 8, &[4, 2, 1])]
    #[case(8, 8, &[8])]
    #[case(13, 8, &[8, 4, 1])]
    #[case(21, 10, &[8, 8, 4, 1])]
    #[case(3, 1, &[1, 1, 1])]
    #[case(3, 0, &[1, 1, 1])]
    fn power_of_two_chunks_cover_every_item_in_order(
        #[case] count: usize,
        #[case] max_len: usize,
        #[case] runs: &[usize],
    ) {
        let items: Vec<usize> = (0..count).collect();
        let chunks: Vec<&[usize]> = power_of_two_chunks(&items, max_len).collect();
        assert_eq!(chunks.iter().map(|c| c.len()).collect::<Vec<_>>(), runs);
        assert_eq!(chunks.concat(), items);
    }

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
