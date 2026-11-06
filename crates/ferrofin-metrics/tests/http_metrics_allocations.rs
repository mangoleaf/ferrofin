//! A request records its `http_*` metrics without touching the heap.
//!
//! [`track_http`] runs on every request while metrics are enabled. It used to
//! copy the matched path twice, copy the controller, action and endpoint label
//! values into a fresh vector and format the status code: about eight
//! allocations a request. This binary counts the heap allocations a request
//! makes on its own thread, with its own global allocator, once through
//! [`track_http`] and once through a pass-through layer in its place: the
//! difference is the middleware's own. Keep this binary to one test: the
//! counter is process-wide, and `init` runs once per process.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::extract::Request;
use axum::middleware::{Next, from_fn};
use axum::response::Response;
use axum::routing::get;
use ferrofin_metrics::{RouteLabels, track_http};
use tower::ServiceExt as _;

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

/// The routes, without a metrics layer.
fn routes() -> Router {
    Router::new().route("/Items/{itemId}", get(|| async { "ok" }))
}

/// The layer standing in for [`track_http`] in the baseline.
async fn passthrough(req: Request, next: Next) -> Response {
    next.run(req).await
}

/// The allocations one `GET uri` through `app` makes on this thread.
async fn allocations(app: &Router, uri: &str) -> usize {
    let app = app.clone();
    let request = Request::builder()
        .uri(uri)
        .body(Body::empty())
        .expect("request builds");
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|counting| counting.set(true));
    let response = app.oneshot(request).await;
    COUNTING.with(|counting| counting.set(false));
    assert!(response.is_ok(), "{uri}");
    ALLOCATIONS.load(Ordering::Relaxed)
}

/// A contract route and an unmatched path (404) cost the middleware no
/// allocation once their series exist.
#[tokio::test]
async fn recording_a_request_allocates_nothing() {
    let spec =
        r#"{"paths":{"/Items/{itemId}":{"get":{"tags":["Items"],"operationId":"GetItem"}}}}"#;
    let _handle = ferrofin_metrics::init(
        RouteLabels::from_openapi_spec(spec, str::to_owned),
        tokio::runtime::Handle::current(),
    )
    .expect("metrics init");
    let tracked = routes().layer(from_fn(track_http));
    let plain = routes().layer(from_fn(passthrough));
    for uri in ["/Items/abc-123", "/nope"] {
        // The first request creates its series; later ones only update them.
        allocations(&tracked, uri).await;
        allocations(&plain, uri).await;
        let baseline = allocations(&plain, uri).await;
        assert_eq!(
            allocations(&tracked, uri).await,
            baseline,
            "{uri}: track_http allocated more than a pass-through layer \
             (its own labels, or the SDK's update of an existing series)"
        );
    }
}
