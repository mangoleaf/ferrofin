//! An M3U playlist is parsed as it arrives, never held whole.
//!
//! `M3uParser.Parse` reads the response stream line by line
//! (`ReadAllLinesAsync` over `ResponseHeadersRead`, v12.2 M3uParser.cs:42-45,
//! 63-69,77), so only the current line and the channels found so far live in
//! memory. This binary counts every heap allocation with its own global
//! allocator and checks that reading a playlist far larger than the limit
//! below never holds more than that limit at once. It holds one test, so no
//! other test's allocations are counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_livetv::fetch::ReqwestFetcher;
use ferrofin_livetv::tuner_host::{M3uTunerHost, TunerHost};
use ferrofin_model::live_tv::TunerHostInfo;

/// The heap bytes allocated right now.
static LIVE: AtomicUsize = AtomicUsize::new(0);

/// The most heap bytes allocated at once since the last reset.
static PEAK: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting what is live.
struct Counting;

impl Counting {
    fn grew(by: usize) {
        let now = LIVE.fetch_add(by, Ordering::SeqCst) + by;
        PEAK.fetch_max(now, Ordering::SeqCst);
    }

    fn shrank(by: usize) {
        LIVE.fetch_sub(by, Ordering::SeqCst);
    }
}

// SAFETY: every call is forwarded unchanged to `System`; the counters are
// atomics and never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            Self::grew(layout.size());
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            Self::grew(layout.size());
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
        Self::shrank(layout.size());
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        let moved = unsafe { System.realloc(ptr, layout, new_size) };
        if !moved.is_null() {
            if new_size >= layout.size() {
                Self::grew(new_size - layout.size());
            } else {
                Self::shrank(layout.size() - new_size);
            }
        }
        moved
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The playlist's size: comfortably more than [`PEAK_LIMIT`].
const BODY_BYTES: usize = 32 * 1024 * 1024;

/// The most the read may hold at once. A reader that buffers the body holds
/// all of [`BODY_BYTES`] (more, while it grows), one that streams holds a few
/// socket buffers.
const PEAK_LIMIT: usize = 4 * 1024 * 1024;

/// A filler line a real playlist carries between its entries.
const FILLER: &[u8] = b"#EXTVLCOPT:network-caching=1000\n";

/// Serves one playlist of about [`BODY_BYTES`]: `#EXTM3U`, filler lines, then
/// one channel entry. The body is written from one reused block, so the peer
/// itself holds almost nothing.
async fn serve_large_playlist() -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}/big.m3u", listener.local_addr().expect("addr"));
    let block: Vec<u8> = FILLER
        .iter()
        .copied()
        .cycle()
        .take(FILLER.len() * 2048)
        .collect();
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await;
        let head = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n#EXTM3U\n";
        if socket.write_all(head).await.is_err() {
            return;
        }
        let mut sent = 0;
        while sent < BODY_BYTES {
            if socket.write_all(&block).await.is_err() {
                return;
            }
            sent += block.len();
        }
        let entry = b"#EXTINF:-1 tvg-id=\"big.example\",Big One\nhttp://tuner/big.ts\n";
        let _ = socket.write_all(entry).await;
    });
    url
}

#[tokio::test]
async fn a_large_playlist_is_parsed_without_holding_it_whole() {
    let url = serve_large_playlist().await;
    let host = M3uTunerHost::new(Arc::new(ReqwestFetcher::new()));
    let tuner = TunerHostInfo {
        url: Some(url),
        ..TunerHostInfo::default()
    };

    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let channels = host.get_channels(&tuner).await.expect("lineup");
    let held = PEAK.load(Ordering::SeqCst).saturating_sub(before);

    assert_eq!(channels.len(), 1);
    assert_eq!(channels[0].name, "Big One");
    assert_eq!(channels[0].url, "http://tuner/big.ts");
    assert!(
        held < PEAK_LIMIT,
        "reading a {BODY_BYTES}-byte playlist held {held} bytes at once (limit {PEAK_LIMIT})"
    );
}
