//! An XMLTV guide is downloaded, cached and parsed as it arrives, never held
//! whole.
//!
//! `XmlTvListingsProvider.GetXml` streams the response (or the local file)
//! into the cache file through `UnzipIfNeededAndCopy`, gunzipping a `.gz` on
//! the way, and `XmlTvReader` reads that file as a stream (v12.2
//! XmlTvListingsProvider.cs:100-123,175-202). This binary counts every heap
//! allocation with its own global allocator and checks that downloading and
//! reading a guide far larger than the limit below never holds more than that
//! limit at once. It holds one test, so no other test's allocations are
//! counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_livetv::fetch::{ReqwestFetcher, SourceFetcher};
use ferrofin_livetv::xmltv::read_xmltv_where;
use ferrofin_livetv::xmltv_cache::XmlTvCache;
use ferrofin_model::live_tv::ListingsProviderInfo;

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

/// The guide's size: comfortably more than [`PEAK_LIMIT`].
const BODY_BYTES: usize = 32 * 1024 * 1024;

/// The most the download and the read may hold at once. A reader that buffers
/// the guide holds all of [`BODY_BYTES`] (twice over while a gzipped one is
/// decompressed), one that streams holds a few buffers.
const PEAK_LIMIT: usize = 4 * 1024 * 1024;

/// One airing the read drops as soon as it is read.
const PROGRAMME: &[u8] = b"<programme start=\"20260725060000 +0000\" \
    stop=\"20260725070000 +0000\" channel=\"filler.tv\"><title>Filler</title>\
    <desc>An airing nobody mapped.</desc></programme>\n";

/// The block the guide's middle is made of, written again and again.
fn block() -> Vec<u8> {
    PROGRAMME
        .iter()
        .copied()
        .cycle()
        .take(PROGRAMME.len() * 1024)
        .collect()
}

/// The whole guide, `<tv>`, about [`BODY_BYTES`] of airings, then one channel,
/// handed to `write` a block at a time.
fn write_guide(mut write: impl FnMut(&[u8])) {
    let block = block();
    write(b"<tv>\n");
    let mut sent = 0;
    while sent < BODY_BYTES {
        write(&block);
        sent += block.len();
    }
    write(b"<channel id=\"big.tv\"><display-name>Big One</display-name></channel>\n</tv>\n");
}

/// Serves `body` at `path` once; the guide's plain form is written from one
/// reused block, so the peer itself holds almost nothing.
async fn serve(path: &'static str, gzipped: Option<Arc<Vec<u8>>>) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let url = format!("http://{}/{path}", listener.local_addr().expect("addr"));
    tokio::spawn(async move {
        let Ok((mut socket, _)) = listener.accept().await else {
            return;
        };
        let mut request = [0_u8; 4096];
        let _ = socket.read(&mut request).await;
        let head = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
        if socket.write_all(head).await.is_err() {
            return;
        }
        if let Some(body) = gzipped {
            let _ = socket.write_all(&body).await;
            return;
        }
        let block = block();
        let _ = socket.write_all(b"<tv>\n").await;
        let mut sent = 0;
        while sent < BODY_BYTES {
            if socket.write_all(&block).await.is_err() {
                return;
            }
            sent += block.len();
        }
        let _ = socket
            .write_all(
                b"<channel id=\"big.tv\"><display-name>Big One</display-name></channel>\n</tv>\n",
            )
            .await;
    });
    url
}

/// Downloads the guide at `url` through the shared cache and reads its
/// channels, answering the most it held at once.
async fn download_and_read(cache: &XmlTvCache, url: String) -> usize {
    let info = ListingsProviderInfo {
        id: Some(uuid::Uuid::new_v4().simple().to_string()),
        path: Some(url),
        ..ListingsProviderInfo::default()
    };
    let before = LIVE.load(Ordering::SeqCst);
    PEAK.store(before, Ordering::SeqCst);
    let document = cache.get_xml(&info).await.expect("guide");
    let guide = tokio::task::spawn_blocking(move || {
        read_xmltv_where(document.reader().expect("reader"), |_| false)
    })
    .await
    .expect("parse");
    let held = PEAK.load(Ordering::SeqCst).saturating_sub(before);

    assert_eq!(guide.error, None);
    assert_eq!(guide.channels.len(), 1);
    assert_eq!(guide.channels[0].display_name, "Big One");
    assert!(guide.programmes.is_empty());
    held
}

#[tokio::test]
async fn a_large_guide_is_cached_and_read_without_holding_it_whole() {
    let dir = tempfile::tempdir().expect("tempdir");
    let cache = XmlTvCache::new(
        Arc::new(ReqwestFetcher::new()) as Arc<dyn SourceFetcher>,
        dir.path(),
    );

    let held = download_and_read(&cache, serve("guide.xml", None).await).await;
    assert!(
        held < PEAK_LIMIT,
        "a {BODY_BYTES}-byte guide held {held} bytes at once (limit {PEAK_LIMIT})"
    );

    // Compressed before the count starts; it is about a thousandth of the guide.
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    write_guide(|bytes| encoder.write_all(bytes).expect("compress"));
    let gzipped = Arc::new(encoder.finish().expect("finish"));
    let held = download_and_read(&cache, serve("guide.xml.gz", Some(gzipped)).await).await;
    assert!(
        held < PEAK_LIMIT,
        "a gzipped {BODY_BYTES}-byte guide held {held} bytes at once (limit {PEAK_LIMIT})"
    );
}
