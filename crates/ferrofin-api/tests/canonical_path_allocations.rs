//! A request whose path needs no rewriting pays no allocation for it.
//!
//! [`canonicalize_path`] runs on every request, and most paths are already
//! canonical: every image, segment and `/web/*` asset among them. It used to
//! split the path into a vector, walk the route table and build the canonical
//! spelling just to find it was the path it started from. This binary counts the
//! heap allocations the call makes on its own thread, with its own global
//! allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_api::routes::canonicalize_path;

thread_local! {
    /// Whether this thread's allocations are being counted.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
}

/// Allocations (and reallocations) made by a counting thread.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting what the current thread allocates while
/// [`COUNTING`] is set.
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

/// The allocations `canonicalize_path(path)` makes, and its answer.
fn canonicalize_counting(path: &str) -> (Option<String>, usize) {
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|counting| counting.set(true));
    let canonical = canonicalize_path(path);
    COUNTING.with(|counting| counting.set(false));
    (canonical, ALLOCATIONS.load(Ordering::Relaxed))
}

/// A canonical API path, a `/web/*` asset and an unknown path answer `None`
/// without touching the heap.
#[test]
fn a_path_needing_no_rewrite_allocates_nothing() {
    // The first call builds the route table, once per process.
    let _ = canonicalize_path("/System/Info/Public");
    for path in [
        "/System/Info/Public",
        "/Users/Me",
        "/Items",
        "/Items/AbC-123/Images/Primary",
        "/Videos/AbC-123/hls1/main/segment.mp4",
        "/web/ConfigurationPage",
        "/web/index.html",
        "/web/assets/img/banner-light.png",
        "/health",
        "/definitely/not/a/route",
        "/",
    ] {
        assert_eq!(canonicalize_counting(path), (None, 0), "{path}");
    }
}

/// A path that does need rewriting still gets its canonical spelling.
#[test]
fn a_path_needing_a_rewrite_is_still_rewritten() {
    assert_eq!(
        canonicalize_path("/items/AbC-123/images/").as_deref(),
        Some("/Items/AbC-123/Images")
    );
}
