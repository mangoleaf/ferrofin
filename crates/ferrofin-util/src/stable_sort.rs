//! A stable sort that tolerates a comparer that is not a total order.
//!
//! Jellyfin sorts with LINQ's `OrderBy`, which is stable and never throws,
//! whatever its comparer returns. Some of the comparers Ferrofin ports are
//! not total orders: `AiredEpisodeOrderComparer` calls an undated episode
//! equal to two dated ones it orders between them. `slice::sort_by` may panic
//! on such a comparer since Rust 1.81, so those sorts use [`stable_sort_by`].

use std::cmp::Ordering;

/// Sorts `items` stably by `compare`, never panicking whatever `compare`
/// returns: a top-down merge sort, O(n log n) comparisons, that takes the
/// right-hand element of a merge only when `compare` puts the left-hand one
/// after it. For a total order the result is the one `slice::sort_by` gives;
/// for any other comparer every element is still kept exactly once.
///
/// The merge sort runs over the items' positions, and each item is then moved
/// once into its place, so large rows are not copied at every merge level.
pub fn stable_sort_by<T>(items: &mut Vec<T>, mut compare: impl FnMut(&T, &T) -> Ordering) {
    if items.len() < 2 {
        return;
    }
    let order = merge_sort((0..items.len()).collect(), &mut |&a: &usize, &b: &usize| {
        compare(&items[a], &items[b])
    });
    let mut slots: Vec<Option<T>> = std::mem::take(items).into_iter().map(Some).collect();
    *items = order
        .into_iter()
        .filter_map(|position| slots[position].take())
        .collect();
}

fn merge_sort<T>(mut items: Vec<T>, compare: &mut impl FnMut(&T, &T) -> Ordering) -> Vec<T> {
    if items.len() <= 1 {
        return items;
    }
    let right = items.split_off(items.len() / 2);
    let left = merge_sort(items, compare);
    let right = merge_sort(right, compare);
    let mut merged = Vec::with_capacity(left.len() + right.len());
    let mut left = left.into_iter().peekable();
    let mut right = right.into_iter().peekable();
    loop {
        let take_right = match (left.peek(), right.peek()) {
            (Some(l), Some(r)) => compare(l, r) == Ordering::Greater,
            (Some(_), None) => false,
            (None, Some(_)) => true,
            (None, None) => break,
        };
        let next = if take_right {
            right.next()
        } else {
            left.next()
        };
        merged.extend(next);
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{Rng, SeedableRng};
    use rstest::rstest;

    #[rstest]
    #[case::empty(vec![], vec![])]
    #[case::one(vec![(1, 'a')], vec![(1, 'a')])]
    #[case::sorted(vec![(1, 'a'), (2, 'b'), (3, 'c')], vec![(1, 'a'), (2, 'b'), (3, 'c')])]
    #[case::reversed(vec![(3, 'a'), (2, 'b'), (1, 'c')], vec![(1, 'c'), (2, 'b'), (3, 'a')])]
    #[case::ties_keep_input_order(
        vec![(2, 'a'), (1, 'b'), (2, 'c'), (1, 'd'), (2, 'e')],
        vec![(1, 'b'), (1, 'd'), (2, 'a'), (2, 'c'), (2, 'e')]
    )]
    fn sorts_by_the_key_and_keeps_ties_in_input_order(
        #[case] mut items: Vec<(u8, char)>,
        #[case] expected: Vec<(u8, char)>,
    ) {
        stable_sort_by(&mut items, |a, b| a.0.cmp(&b.0));
        assert_eq!(items, expected);
    }

    #[test]
    fn a_total_order_gives_what_the_standard_stable_sort_gives() {
        let mut rng = StdRng::seed_from_u64(905);
        for len in 0..200 {
            let items: Vec<(u8, usize)> = (0..len).map(|i| (rng.gen_range(0..8), i)).collect();
            let mut ours = items.clone();
            let mut std_sorted = items;
            stable_sort_by(&mut ours, |a, b| a.0.cmp(&b.0));
            std_sorted.sort_by_key(|item| item.0);
            assert_eq!(ours, std_sorted, "length {len}");
        }
    }

    #[test]
    fn a_comparer_that_is_no_order_at_all_keeps_every_element() {
        let mut rng = StdRng::seed_from_u64(905);
        for len in 0..200 {
            let mut items: Vec<usize> = (0..len).collect();
            let mut verdicts = StdRng::seed_from_u64(rng.r#gen());
            stable_sort_by(&mut items, |_, _| match verdicts.gen_range(0..3) {
                0 => Ordering::Less,
                1 => Ordering::Equal,
                _ => Ordering::Greater,
            });
            items.sort_unstable();
            assert_eq!(items, (0..len).collect::<Vec<_>>(), "length {len}");
        }
    }

    /// The shape gl-905 reported: rows ordered by a date, where a row without
    /// one is equal to every other row. `slice::sort_by` panics on this input.
    #[test]
    fn an_undated_row_among_dated_ones_sorts_without_panicking() {
        let mut days: Vec<Option<u8>> = (11..=20)
            .rev()
            .map(Some)
            .chain([None])
            .chain((0..=10).rev().map(Some))
            .collect();
        stable_sort_by(&mut days, |a, b| match (a, b) {
            (Some(a), Some(b)) => a.cmp(b),
            _ => Ordering::Equal,
        });
        let dated: Vec<u8> = days.iter().flatten().copied().collect();
        assert_eq!(dated, (0..=20).collect::<Vec<_>>());
        assert_eq!(days.iter().filter(|day| day.is_none()).count(), 1);
    }
}
