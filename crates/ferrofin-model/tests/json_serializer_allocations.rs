//! Writing a guid or a date to JSON allocates nothing.
//!
//! Every DTO guid and date goes through the `ferrofin_model::json`
//! serializers (`JsonGuidConverter`, `JsonNullableGuidConverter`,
//! `JsonDateTimeConverter`). Building each guid's `ToString("N")` text in a
//! fresh `String` cost one heap allocation, and each date's text seven: 29
//! for the episode `BaseItemDto` below, on every item of every list response.
//! This binary counts the heap allocations serialization makes on its own
//! thread, with its own global allocator, into an output buffer sized up front
//! so that only the serializers themselves can allocate.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, TimeZone, Utc};
use ferrofin_model::data::BaseItemKind;
use ferrofin_model::dto::{BaseItemDto, UserItemDataDto};
use serde::Serialize;
use uuid::Uuid;

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

/// Serializes `value` as JSON into a buffer with room to spare, returning the
/// text and the allocations serialization made.
fn serialize_counting<T: Serialize>(value: &T) -> (String, usize) {
    let mut out = Vec::with_capacity(1 << 20);
    ALLOCATIONS.store(0, Ordering::Relaxed);
    COUNTING.with(|counting| counting.set(true));
    let written = serde_json::to_writer(&mut out, value);
    COUNTING.with(|counting| counting.set(false));
    let allocations = ALLOCATIONS.load(Ordering::Relaxed);
    written.expect("serialize");
    (String::from_utf8(out).expect("JSON is UTF-8"), allocations)
}

const ID: Uuid = Uuid::from_u128(0xd37e_cb9d_75b0_c0a8_e9ec_b0a8_64ec_670e);

/// One field per serializer, with the values that take each branch.
#[derive(Serialize)]
struct EveryField {
    #[serde(with = "ferrofin_model::json::guid")]
    id: Uuid,
    #[serde(with = "ferrofin_model::json::guid")]
    empty: Uuid,
    #[serde(with = "ferrofin_model::json::guid::option")]
    parent: Option<Uuid>,
    #[serde(with = "ferrofin_model::json::guid::option")]
    nil_parent: Option<Uuid>,
    #[serde(with = "ferrofin_model::json::guid::option")]
    no_parent: Option<Uuid>,
    #[serde(with = "ferrofin_model::json::guid::vec")]
    children: Vec<Uuid>,
    #[serde(with = "ferrofin_model::json::guid::option_vec")]
    extra: Option<Vec<Uuid>>,
    #[serde(with = "ferrofin_model::json::guid::option_vec")]
    no_extra: Option<Vec<Uuid>>,
    #[serde(with = "ferrofin_model::json::datetime")]
    at: DateTime<Utc>,
    #[serde(with = "ferrofin_model::json::datetime")]
    precise: DateTime<Utc>,
    #[serde(with = "ferrofin_model::json::datetime::option")]
    maybe: Option<DateTime<Utc>>,
    #[serde(with = "ferrofin_model::json::datetime::option")]
    never: Option<DateTime<Utc>>,
}

#[test]
fn guid_and_date_serializers_allocate_nothing() {
    let at = Utc.with_ymd_and_hms(2022, 1, 1, 0, 0, 0).unwrap();
    let fields = EveryField {
        id: ID,
        empty: Uuid::nil(),
        parent: Some(ID),
        nil_parent: Some(Uuid::nil()),
        no_parent: None,
        children: vec![ID, Uuid::from_u128(1)],
        extra: Some(vec![ID]),
        no_extra: None,
        at,
        precise: at + chrono::Duration::nanoseconds(764_472_900),
        maybe: Some(at + chrono::Duration::milliseconds(200)),
        never: None,
    };

    let (json, allocations) = serialize_counting(&fields);

    assert_eq!(
        json,
        concat!(
            r#"{"id":"d37ecb9d75b0c0a8e9ecb0a864ec670e","#,
            r#""empty":"00000000000000000000000000000000","#,
            r#""parent":"d37ecb9d75b0c0a8e9ecb0a864ec670e","nil_parent":null,"no_parent":null,"#,
            r#""children":["d37ecb9d75b0c0a8e9ecb0a864ec670e","00000000000000000000000000000001"],"#,
            r#""extra":["d37ecb9d75b0c0a8e9ecb0a864ec670e"],"no_extra":null,"#,
            r#""at":"2022-01-01T00:00:00.0000000Z","precise":"2022-01-01T00:00:00.7644729Z","#,
            r#""maybe":"2022-01-01T00:00:00.2Z","never":null}"#,
        )
    );
    assert_eq!(
        allocations, 0,
        "serializing 8 guids and 3 dates made {allocations} allocations"
    );
}

/// The items of a list response, with every guid and date a library item
/// commonly carries (an episode with its series, season and user data).
fn page(count: u128) -> Vec<BaseItemDto> {
    let added = Utc.with_ymd_and_hms(2026, 8, 23, 16, 57, 22).unwrap()
        + chrono::Duration::nanoseconds(479_586_000);
    let aired = Utc.with_ymd_and_hms(2019, 4, 14, 0, 0, 0).unwrap();
    (0..count)
        .map(|n| {
            let id = Uuid::from_u128(ID.as_u128() ^ n);
            BaseItemDto {
                name: Some(format!("Episode {n}")),
                server_id: Some("f0e1d2c3b4a5968778695a4b3c2d1e0f".to_owned()),
                id,
                date_created: Some(added),
                premiere_date: Some(aired),
                parent_id: Some(Uuid::from_u128(n + 1)),
                series_id: Some(Uuid::from_u128(n + 2)),
                season_id: Some(Uuid::from_u128(n + 3)),
                parent_backdrop_item_id: Some(Uuid::from_u128(n + 2)),
                parent_logo_item_id: Some(Uuid::from_u128(n + 2)),
                parent_thumb_item_id: Some(Uuid::from_u128(n + 2)),
                channel_id: Some(Uuid::nil()),
                type_: BaseItemKind::Episode,
                user_data: Some(UserItemDataDto {
                    rating: None,
                    played_percentage: None,
                    unplayed_item_count: None,
                    playback_position_ticks: 0,
                    play_count: 1,
                    is_favorite: false,
                    likes: None,
                    last_played_date: Some(added),
                    played: true,
                    key: format!("{n}"),
                    item_id: id,
                }),
                ..Default::default()
            }
        })
        .collect()
}

#[test]
fn a_page_of_items_writes_its_guids_and_dates_without_allocating() {
    const ITEMS: u128 = 100;
    let items = page(ITEMS);

    let (json, allocations) = serialize_counting(&items);

    let first: serde_json::Value = serde_json::from_str(&json).expect("JSON");
    let first = &first[0];
    assert_eq!(first["Id"], "d37ecb9d75b0c0a8e9ecb0a864ec670e");
    assert_eq!(first["DateCreated"], "2026-08-23T16:57:22.479586Z");
    assert_eq!(first["PremiereDate"], "2019-04-14T00:00:00.0000000Z");
    assert_eq!(first["ParentId"], "00000000000000000000000000000001");
    assert!(
        first
            .get("ChannelId")
            .is_some_and(serde_json::Value::is_null)
    );
    assert_eq!(first["UserData"]["ItemId"], first["Id"]);
    assert_eq!(first["UserData"]["LastPlayedDate"], first["DateCreated"]);
    assert_eq!(
        allocations, 0,
        "serializing {ITEMS} items made {allocations} allocations"
    );
}
