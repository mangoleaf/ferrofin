//! A folder listing sorted by name computes each row's sort key once.
//!
//! A non-recursive folder listing sorted by `SortName` or `Name` is sorted in
//! memory with `SortNameComparer`/`NameComparer`'s ordinal case-insensitive
//! comparison (`sort_folder_children`). Upper-casing both names on every
//! comparison made about four `String`s per comparison, n·log2(n) of them
//! for the whole unpaged child list: roughly 90,000 for a 2,000-child folder,
//! on every request. This binary counts the heap allocations the sort makes
//! on its own thread, with its own global allocator, and checks that they
//! grow with the number of rows, not with the number of comparisons.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_core::FerrofinItemRepository;
use ferrofin_core::item_type_lookup::ItemTypeLookup;
use ferrofin_db::Database;
use ferrofin_db::entities::base_items::BaseItemEntity;
use ferrofin_model::dto::SortOrder;
use ferrofin_model::live_tv::ItemSortBy;
use ferrofin_traits::options::InternalItemsQuery;
use ferrofin_traits::persistence::ItemRepository;

thread_local! {
    /// Whether this thread's allocations are being counted.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

/// Allocations (and reallocations) made by a counting thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting what the current thread allocates while
/// [`COUNTING`] is set. The database's worker threads are never counted.
struct Counting;

impl Counting {
    fn count() {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
}

// SAFETY: every call is forwarded unchanged to `System`; the counter is an
// atomic and the flag a const-initialised thread-local, neither allocates.
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

/// The folder's size: the issue's 2,000-child example.
const ROWS: usize = 2_000;

/// The most allocations the sort may make per row. Computing a row's keys
/// once costs one copy of its sort name and one upper-cased copy of each
/// name key, so three for `SortName,Name`; the vector of keyed rows and the
/// sort's scratch buffer are a few more for the whole list. A comparator
/// that upper-cases on every comparison costs about four per comparison,
/// some 4·log2(ROWS) ≈ 44 per row here.
const PER_ROW_BUDGET: usize = 4;

/// `count` children of one folder, in a scrambled order, with mixed-case
/// names whose sort names tie in pairs, so both keys are compared.
fn children(count: usize) -> Vec<BaseItemEntity> {
    (0..count)
        .map(|n| {
            // Visit the numbers in a scrambled order (7919 is prime and does
            // not divide ROWS), so the sort has real work to do.
            let key = (n * 7919) % count;
            let name = if key.is_multiple_of(2) {
                format!("child {key:05}")
            } else {
                format!("Child {key:05}")
            };
            BaseItemEntity {
                type_: "MediaBrowser.Controller.Entities.Video".to_owned(),
                sort_name: Some(format!("child {:05}", key / 2)),
                name: Some(name),
                ..Default::default()
            }
        })
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn sorting_a_folder_by_name_allocates_per_row_not_per_comparison() {
    let db = Database::connect_in_memory().await.expect("connect");
    let repo = FerrofinItemRepository::new(db, Arc::new(ItemTypeLookup::new()));
    let query = InternalItemsQuery {
        order_by: vec![
            (ItemSortBy::SortName, SortOrder::Ascending),
            (ItemSortBy::Name, SortOrder::Descending),
        ],
        ..Default::default()
    };
    let rows = children(ROWS);

    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|counting| counting.set(true));
    let sorted = repo.sort_folder_children(rows, &query).await;
    COUNTING.with(|counting| counting.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);

    let sorted = sorted.expect("sort");
    assert_eq!(sorted.len(), ROWS);
    // Sort names ascending; within each tied pair, names descending,
    // ignoring case.
    for pair in sorted.windows(2) {
        let upper = |row: &BaseItemEntity| row.name.as_deref().map(str::to_ascii_uppercase);
        assert!(
            pair[0].sort_name < pair[1].sort_name
                || (pair[0].sort_name == pair[1].sort_name && upper(&pair[0]) > upper(&pair[1])),
            "{:?} before {:?}",
            pair[0].name,
            pair[1].name
        );
    }
    assert!(
        allocations <= PER_ROW_BUDGET * ROWS,
        "sorting {ROWS} rows made {allocations} allocations, more than \
         {PER_ROW_BUDGET} per row: the comparator is allocating"
    );
}
