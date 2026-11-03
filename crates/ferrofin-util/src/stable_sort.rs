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
/// The merge sort runs over the items' positions, merging through one scratch
/// buffer, and the items are then swapped into place along the cycles of the
/// resulting permutation: at most two allocations whatever the length, and
/// large rows are not copied at every merge level.
pub fn stable_sort_by<T>(items: &mut [T], mut compare: impl FnMut(&T, &T) -> Ordering) {
    if items.len() < 2 {
        return;
    }
    let mut order: Vec<usize> = (0..items.len()).collect();
    let mut scratch = vec![0; items.len()];
    merge_sort(&mut order, &mut scratch, &mut |a, b| {
        compare(&items[a], &items[b])
    });
    move_into_place(items, &mut order);
}

/// Sorts `run` by `compare`: its first `run.len() / 2` positions, then the
/// rest, then the two halves merged through `scratch`, which is as long as
/// `run`. While both halves have positions left, a merge compares their
/// fronts and takes the right-hand one only on [`Ordering::Greater`]; once
/// either half runs out, the other's rest follows without a comparison.
fn merge_sort(
    run: &mut [usize],
    scratch: &mut [usize],
    compare: &mut impl FnMut(usize, usize) -> Ordering,
) {
    if run.len() < 2 {
        return;
    }
    let middle = run.len() / 2;
    let (left, right) = run.split_at_mut(middle);
    let (left_scratch, right_scratch) = scratch.split_at_mut(middle);
    merge_sort(left, left_scratch, compare);
    merge_sort(right, right_scratch, compare);
    let (mut next_left, mut next_right, mut merged) = (0, middle, 0);
    while next_left < middle && next_right < run.len() {
        scratch[merged] = if compare(run[next_left], run[next_right]) == Ordering::Greater {
            next_right += 1;
            run[next_right - 1]
        } else {
            next_left += 1;
            run[next_left - 1]
        };
        merged += 1;
    }
    // What is left of the left half goes after everything merged; what is
    // left of the right half is already there.
    run.copy_within(next_left..middle, merged);
    run[..merged].copy_from_slice(&scratch[..merged]);
}

/// Puts `items` in the order `order` gives, where `order[place]` is the
/// current position of the item that belongs at `place`: at most one swap per
/// item out of place, following each cycle of the permutation from its lowest
/// place. A place is marked done by setting `order[place]` to `place`.
fn move_into_place<T>(items: &mut [T], order: &mut [usize]) {
    for start in 0..order.len() {
        let mut place = start;
        loop {
            let source = order[place];
            order[place] = place;
            if source == start {
                break;
            }
            items.swap(place, source);
            place = source;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::StdRng;
    use rand::{RngExt as _, SeedableRng};
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
            let items: Vec<(u8, usize)> = (0..len).map(|i| (rng.random_range(0..8), i)).collect();
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
            let mut verdicts = StdRng::seed_from_u64(rng.random());
            stable_sort_by(&mut items, |_, _| match verdicts.random_range(0..3) {
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

    /// The merge sort [`stable_sort_by`] used before gl-1051, which split and
    /// merged through new vectors at every level: the oracle for its order and
    /// its comparisons.
    fn reference_sort_by<T>(items: &mut Vec<T>, mut compare: impl FnMut(&T, &T) -> Ordering) {
        fn merge_sort<T>(
            mut items: Vec<T>,
            compare: &mut impl FnMut(&T, &T) -> Ordering,
        ) -> Vec<T> {
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

    /// Sorts `items` with both sorts by `verdict`, logging each comparison by
    /// the items' ids, and checks they give the same order after the same
    /// comparisons.
    fn assert_same_as_reference<K: Clone + std::fmt::Debug + PartialEq>(
        items: &[(K, String)],
        verdict: impl Fn(&(K, String), &(K, String)) -> Ordering,
    ) {
        let mut ours = items.to_vec();
        let mut ours_log = Vec::new();
        stable_sort_by(&mut ours, |a, b| {
            ours_log.push((a.1.clone(), b.1.clone()));
            verdict(a, b)
        });
        let mut reference = items.to_vec();
        let mut reference_log = Vec::new();
        reference_sort_by(&mut reference, |a, b| {
            reference_log.push((a.1.clone(), b.1.clone()));
            verdict(a, b)
        });
        let len = items.len();
        assert_eq!(ours, reference, "length {len}");
        assert_eq!(ours_log, reference_log, "length {len}");
    }

    /// Lengths around the powers of two, where the halves' split changes
    /// shape, and every length up to a long season list.
    fn lengths() -> impl Iterator<Item = usize> {
        (0..200).chain([255, 256, 257, 511, 512, 513, 1000])
    }

    /// A comparer that is no order at all, but a fixed verdict for each pair:
    /// the new sort makes the comparisons the old one made, in the same
    /// order, and so puts every item where the old one did.
    #[test]
    fn a_comparer_that_is_no_order_at_all_gets_the_previous_sorts_order() {
        let mut rng = StdRng::seed_from_u64(1051);
        for len in lengths() {
            let items: Vec<(usize, String)> =
                (0..len).map(|id| (id, format!("item {id}"))).collect();
            let verdicts: Vec<Ordering> = (0..len * len)
                .map(|_| match rng.random_range(0..3) {
                    0 => Ordering::Less,
                    1 => Ordering::Equal,
                    _ => Ordering::Greater,
                })
                .collect();
            assert_same_as_reference(&items, |a, b| verdicts[a.0 * len + b.0]);
        }
    }

    /// The aired-order shape (an undated row equal to every other row) and a
    /// total order with ties, at random: the same order as the old sort.
    #[test]
    fn undated_rows_and_ties_get_the_previous_sorts_order() {
        let mut rng = StdRng::seed_from_u64(1051);
        for len in lengths() {
            let items: Vec<(Option<u8>, String)> = (0..len)
                .map(|id| {
                    let day = rng.random_range(0..12);
                    ((day < 10).then_some(day), format!("item {id}"))
                })
                .collect();
            assert_same_as_reference(&items, |a, b| match (a.0, b.0) {
                (Some(a), Some(b)) => a.cmp(&b),
                _ => Ordering::Equal,
            });
            assert_same_as_reference(&items, |a, b| a.0.cmp(&b.0));
        }
    }
}
