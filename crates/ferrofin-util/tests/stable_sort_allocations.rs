//! A sort allocates the same few buffers whatever the number of items.
//!
//! [`stable_sort_by`] orders every aired-order season list, every Next Up
//! candidate list and every in-memory folder listing, once per request. It used
//! to split and merge through two new vectors at every node of its merge sort,
//! about two allocations per item (gl-1051). This binary counts the heap
//! allocations the call makes on its own thread, with its own global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use ferrofin_util::stable_sort::stable_sort_by;

thread_local! {
    /// This thread's allocations (and reallocations) while it is counting,
    /// `None` while it is not. Per thread, so tests running side by side in
    /// one process (`cargo test`) never count each other's.
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

/// The system allocator, counting what the current thread allocates while
/// it is counting.
struct Counting;

impl Counting {
    fn count() {
        // `try_with`: the slot is gone while the thread is being torn down.
        let _ = ALLOCATIONS.try_with(|count| {
            if let Some(n) = count.get() {
                count.set(Some(n + 1));
            }
        });
    }
}

// SAFETY: every call is forwarded unchanged to `System`; the counter is a
// const-initialised thread-local `Cell`, which never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::count();
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The allocations sorting `items` by its key makes, and the sorted items.
fn sort_counting(mut items: Vec<(u32, usize)>) -> (Vec<(u32, usize)>, usize) {
    ALLOCATIONS.with(|count| count.set(Some(0)));
    stable_sort_by(&mut items, |a, b| a.0.cmp(&b.0));
    let allocations = ALLOCATIONS
        .with(|count| count.replace(None))
        .unwrap_or_default();
    (items, allocations)
}

/// Items in descending key order with runs of ties, so every merge compares.
fn descending_with_ties(len: usize) -> Vec<(u32, usize)> {
    (0..len)
        .map(|position| (u32::try_from((len - position) / 3).expect("key"), position))
        .collect()
}

/// Sorting two items and sorting thousands make the same two allocations (the
/// items' positions and the merge's scratch buffer), and the sort still orders
/// them stably.
#[test]
fn a_sort_allocates_the_same_whatever_the_length() {
    let (_, two) = sort_counting(descending_with_ties(2));
    assert_eq!(two, 2, "sorting 2 items made {two} allocations");
    for len in [3, 10, 100, 1000, 4096] {
        let items = descending_with_ties(len);
        let mut expected = items.clone();
        expected.sort_by_key(|item| item.0);
        let (sorted, allocations) = sort_counting(items);
        assert_eq!(sorted, expected, "length {len}");
        assert_eq!(
            allocations, two,
            "sorting {len} items made {allocations} allocations, sorting 2 made {two}"
        );
    }
}

/// A list too short to need sorting is left alone without touching the heap.
#[test]
fn a_list_of_one_or_none_allocates_nothing() {
    assert_eq!(sort_counting(Vec::new()), (Vec::new(), 0));
    assert_eq!(sort_counting(vec![(7, 0)]), (vec![(7, 0)], 0));
}
