//! Forwarded headers are read only as far as the walk over them goes.
//!
//! The IP gate resolves the client of every API request before
//! authentication, and the scheme and host walk runs for every published
//! address. Both used to split the whole `X-Forwarded-*` header into owned
//! strings first, about 27 times the header's size, and then throw them away
//! when the peer was not a known proxy, which an anonymous request could
//! choose. This binary counts the bytes each call allocates on its own thread,
//! with its own global allocator.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::net::IpAddr;
use std::sync::{Arc, RwLock};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use ferrofin_api::forwarded_headers::{client_address_through, walk_forwarded_headers};
use ferrofin_api::router::create_router;
use ferrofin_api::test_support::authed_fake_state;
use ferrofin_networking::{NetworkConfiguration, NetworkManager};
use tower::ServiceExt;

thread_local! {
    /// Whether this thread's allocations are being counted.
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    /// Bytes this thread allocated (and reallocated to) while counting: per
    /// thread, so tests running side by side do not count each other's.
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
}

/// The system allocator, counting what the current thread allocates while
/// [`COUNTING`] is set.
struct Counting;

impl Counting {
    fn count(bytes: usize) {
        if COUNTING.try_with(Cell::get).unwrap_or(false) {
            let _ = ALLOCATED.try_with(|allocated| allocated.set(allocated.get() + bytes));
        }
    }
}

// SAFETY: every call is forwarded unchanged to `System`; the counter and the
// flag are const-initialised thread-locals, which do not allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        Self::count(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        Self::count(layout.size());
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        Self::count(new_size);
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The one known proxy.
const PROXY: &str = "10.0.0.1";

/// About 400 KB of `X-Forwarded-For` (hyper's HTTP/1 buffer takes about
/// 408 KB of headers): 200,000 one-letter entries, then a client address.
fn long_chain() -> String {
    let mut chain = "a,".repeat(200_000);
    chain.push_str("203.0.113.9");
    chain
}

/// Starts counting this thread's allocations.
fn start_counting() {
    ALLOCATED.with(|allocated| allocated.set(0));
    COUNTING.with(|counting| counting.set(true));
}

/// Stops counting and answers the bytes allocated since [`start_counting`].
fn stop_counting() -> usize {
    COUNTING.with(|counting| counting.set(false));
    ALLOCATED.with(Cell::get)
}

/// The bytes allocated while `GET /System/Ping`, an anonymous route, is
/// answered for `peer` with `forwarded_for` as its `X-Forwarded-For`.
async fn ping_allocations(peer: &str, forwarded_for: &str) -> usize {
    let network = NetworkManager::with_defaults(
        NetworkConfiguration {
            known_proxies: vec![PROXY.to_owned()],
            ..NetworkConfiguration::default()
        },
        "",
    );
    let app = create_router(authed_fake_state().with_network(Arc::new(RwLock::new(network))));
    let mut request = Request::builder()
        .uri("/System/Ping")
        .header("X-Forwarded-For", forwarded_for)
        .body(Body::empty())
        .expect("request");
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(std::net::SocketAddr::new(
            peer.parse().expect("peer"),
            50_000,
        )));
    start_counting();
    let response = app.oneshot(request).await.expect("response");
    let allocated = stop_counting();
    assert_eq!(response.status(), StatusCode::OK);
    allocated
}

/// A peer that is not a known proxy gets no say in who the client is, so its
/// `X-Forwarded-For` is never read: a 400 KB header costs the request what a
/// one-entry header does. A known proxy's chain is read from the right only
/// as far as the walk goes, here one entry.
#[rstest::rstest]
#[case::a_stranger("198.51.100.4")]
#[case::a_known_proxy(PROXY)]
#[tokio::test]
async fn the_client_walk_reads_only_the_entries_it_reaches(#[case] peer: &str) {
    let short = ping_allocations(peer, "203.0.113.9").await;
    let long = ping_allocations(peer, &long_chain()).await;
    assert!(
        long <= short + 1024,
        "a 400 KB X-Forwarded-For cost {} more bytes than a one-entry one",
        long.saturating_sub(short)
    );
}

/// The client a 400 KB chain resolves to, read no further than the walk goes:
/// the peer itself without known proxies (forwarding off) or from a stranger,
/// and from a known proxy the client it vouched for, the rightmost entry.
#[rstest::rstest]
#[case::no_known_proxies(&[], PROXY, PROXY)]
#[case::a_stranger(&[PROXY], "198.51.100.4", "198.51.100.4")]
#[case::a_known_proxy(&[PROXY], PROXY, "203.0.113.9")]
fn a_long_chain_resolves_to_the_client_the_walk_reaches(
    #[case] known_proxies: &[&str],
    #[case] peer: &str,
    #[case] expected: &str,
) {
    let network = RwLock::new(NetworkManager::with_defaults(
        NetworkConfiguration {
            known_proxies: known_proxies
                .iter()
                .map(|&proxy| proxy.to_owned())
                .collect(),
            ..NetworkConfiguration::default()
        },
        "",
    ));
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", long_chain().parse().expect("value"));
    let peer: IpAddr = peer.parse().expect("peer");
    start_counting();
    let client = client_address_through(Some(&network), peer, &headers);
    let allocated = stop_counting();
    assert_eq!(client, expected.parse::<IpAddr>().expect("expected"));
    assert!(
        allocated <= 1024,
        "resolving the client allocated {allocated} bytes"
    );
}

/// The scheme and host walk reads the three headers the same way: nothing
/// from a stranger, and from a known proxy only the hops it walks.
#[rstest::rstest]
#[case::a_stranger("198.51.100.4", None)]
#[case::a_known_proxy(PROXY, Some("https"))]
fn the_scheme_and_host_walk_reads_only_the_hops_it_reaches(
    #[case] peer: &str,
    #[case] scheme: Option<&str>,
) {
    let mut headers = HeaderMap::new();
    headers.insert("x-forwarded-for", long_chain().parse().expect("value"));
    let mut protos = "http,".repeat(80_000);
    protos.push_str("https");
    headers.insert("x-forwarded-proto", protos.parse().expect("value"));
    let peer: IpAddr = peer.parse().expect("peer");
    let proxy: IpAddr = PROXY.parse().expect("proxy");
    start_counting();
    let forwarded = walk_forwarded_headers(peer, &headers, |address| address == proxy);
    let allocated = stop_counting();
    assert_eq!(forwarded.scheme.as_deref(), scheme);
    assert!(allocated <= 1024, "the walk allocated {allocated} bytes");
}
