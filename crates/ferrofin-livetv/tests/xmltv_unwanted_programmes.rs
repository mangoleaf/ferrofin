//! A programme the reader does not want is read past, never built.
//!
//! A guide refresh reads a large IPTV guide twice, once for its channels and
//! once for the few bound channels' airings over the guide window, and
//! rejects almost every programme both times. `read_xmltv_where`'s `keep`
//! judges a programme on its start tag, so a rejected one's titles,
//! descriptions, categories, icons and ratings are never allocated. This
//! binary counts heap allocations with its own global allocator. It holds one
//! test, so no other test's allocations are counted.

use std::alloc::{GlobalAlloc, Layout, System};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};

use ferrofin_livetv::xmltv::read_xmltv_where;

/// Heap allocations (and reallocations) made so far.
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

/// The system allocator, counting the allocations it makes.
struct Counting;

// SAFETY: every call is forwarded unchanged to `System`; the counter is an
// atomic and never allocates.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the caller upholds `GlobalAlloc::alloc_zeroed`'s contract.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller upholds `GlobalAlloc::dealloc`'s contract.
        unsafe { System.dealloc(ptr, layout) };
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::SeqCst);
        // SAFETY: the caller upholds `GlobalAlloc::realloc`'s contract.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The airings in each guide.
const AIRINGS: usize = 2_000;

/// What a real guide's airing carries: the children the reader builds a
/// programme from.
const CHILDREN: &str = r#"<title lang="en">Filler</title><title lang="fr">Remplissage</title>
<sub-title>Part One</sub-title><desc lang="en">An airing nobody mapped.</desc>
<category lang="en">News</category><category lang="en">Talk</category>
<episode-num system="xmltv_ns">1.2.</episode-num><episode-num system="onscreen">S02E03</episode-num>
<icon src="http://guide.invalid/icon.png"/><rating system="VCHIP"><value>TV-G</value></rating>
<previously-shown start="20200101000000 +0000"/><new/><date>2020</date>
<image type="backdrop">http://guide.invalid/backdrop.jpg</image>"#;

/// A guide of [`AIRINGS`] airings on a channel nobody bound, each holding
/// `children`, followed by the one channel that is bound.
fn guide(children: &str) -> String {
    let mut xml = String::from("<tv>\n");
    for hour in 0..AIRINGS {
        let _ = writeln!(
            xml,
            "<programme start=\"20260725{:02}0000 +0000\" stop=\"20260725{:02}0000 +0000\" \
             channel=\"unbound.tv\">{children}</programme>",
            hour % 24,
            (hour + 1) % 24,
        );
    }
    xml.push_str("<channel id=\"bound.tv\"><display-name>Bound</display-name></channel>\n</tv>\n");
    xml
}

/// The allocations a refresh's read of `xml` makes, keeping only the bound
/// channel's airings.
fn allocations_reading(xml: &str) -> usize {
    let before = ALLOCATIONS.load(Ordering::SeqCst);
    let guide = read_xmltv_where(xml.as_bytes(), None, |p| p.channel_id == "bound.tv");
    let made = ALLOCATIONS.load(Ordering::SeqCst) - before;
    assert_eq!(guide.error, None);
    assert!(guide.programmes.is_empty(), "{:?}", guide.programmes);
    assert_eq!(guide.channels.len(), 1);
    made
}

#[test]
fn a_rejected_programme_s_children_cost_only_the_well_formedness_check() {
    let bare = guide("");
    let full = guide(CHILDREN);
    let bare_cost = allocations_reading(&bare);
    let full_cost = allocations_reading(&full);
    // Reading past a child still checks it is well-formed, as `XmlReader`
    // does, and quick-xml's duplicate-attribute check allocates once for an
    // element with attributes (each child here has at most one). Building
    // the programme cost about 40 allocations an airing more. The reader's
    // buffers grow a few times for the longer elements.
    let well_formed = AIRINGS * CHILDREN.matches("=\"").count();
    let extra = full_cost.saturating_sub(bare_cost);
    assert!(
        extra <= well_formed + AIRINGS / 10,
        "{AIRINGS} rejected airings' children cost {extra} allocations, {well_formed} of them \
         the well-formedness check's ({bare_cost} without children, {full_cost} with)"
    );
}
