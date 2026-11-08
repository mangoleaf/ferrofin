//! [`ProviderCache`]: a scan's memo of remote provider responses, kept by
//! the rules upstream's provider clients keep theirs by.
//!
//! Upstream holds each response in its provider client's `MemoryCache`
//! (`Microsoft.Extensions.Caching.Memory` 10.0.14, the version Jellyfin
//! 12.4 pins): TheMovieDb's in `TmdbClientManager`
//! (`TmdbClientManager.cs:26-48,771-822`), TheTVDB's in the plugin's
//! `TvdbClientManager` (jellyfin-plugin-tvdb `5c4592f`,
//! `TvdbClientManager.cs:35-61`). The scan keeps the responses it shares
//! between an item and the items after it in one of these, by the same
//! rules (`MemoryCache.cs` `SetEntry`, `PostProcessTryGetValue`,
//! `StartScanForExpiredItemsIfNeeded`, `OvercapacityCompaction`,
//! `Compact`):
//!
//! - An entry expires a fixed time after it was stored
//!   (`AbsoluteExpirationRelativeToNow`), and is never read after that.
//!   The expired entries are swept out by a write once more than
//!   [`EXPIRATION_SCAN_FREQUENCY`] has passed since the last sweep.
//!   Upstream's reads can start that sweep too, and a read of an expired
//!   entry removes it; here only writes sweep (a read holds the cache
//!   shared), so an expired entry keeps its weight until then. That only
//!   frees the memory later.
//! - A read stamps the entry as the most recently used one (`LastAccessed`).
//! - A cache with a size limit weighs each entry (`Size`). An entry that
//!   would take it past the limit is not stored, and the entry it replaced
//!   is removed. The cache then compacts itself: when it holds more than
//!   [`COMPACTION_PERCENT`] below its limit, the expired entries go, then
//!   the least recently used, until it is back there. The next reader asks
//!   the provider again. Upstream compacts on a pool thread just after the
//!   write; here it compacts in the write.

use std::borrow::Borrow;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// How long after the last sweep a write sweeps the expired entries out:
/// `MemoryCacheOptions.ExpirationScanFrequency`'s default, which neither
/// provider client changes.
pub(super) const EXPIRATION_SCAN_FREQUENCY: Duration = Duration::from_mins(1);

/// How far below its size limit, in percent, an over-capacity compaction
/// takes a cache: `MemoryCacheOptions.CompactionPercentage`'s default
/// (`0.05`), which `TmdbClientManager` keeps.
pub(super) const COMPACTION_PERCENT: u64 = 5;

/// A map of provider responses whose entries expire a fixed time after they
/// were stored, optionally bounded by a total weight (see the module docs).
pub(super) struct ProviderCache<K, V> {
    entries: HashMap<K, Entry<V>>,
    /// How long an entry is kept after it was stored.
    duration: Duration,
    /// The bound on the weight held, or `None` for an unbounded cache.
    limit: Option<SizeLimit<V>>,
    /// The weight held (`MemoryCache`'s `Size`); an unbounded cache weighs
    /// every entry `1`.
    size: u64,
    /// When the expired entries were last swept out.
    last_sweep: Instant,
    /// The last use stamp handed out: every store and every read takes the
    /// next one, so the entry used longest ago holds the smallest stamp.
    uses: AtomicU64,
}

/// A [`ProviderCache`]'s size limit (`MemoryCacheOptions.SizeLimit`) and
/// how it weighs an entry (the `Size` each `Set` passes).
struct SizeLimit<V> {
    limit: u64,
    weigh: fn(&V) -> u64,
}

/// One stored response.
struct Entry<V> {
    value: V,
    /// When it expires; `None` when its duration reaches past what an
    /// [`Instant`] can hold.
    expires: Option<Instant>,
    /// Its weight under the cache's size limit.
    weight: u64,
    /// The use stamp of its last store or read.
    last_used: AtomicU64,
}

impl<V> Entry<V> {
    /// `CheckExpired` for an absolute expiration: expired from its expiry
    /// on.
    fn expired(&self, now: Instant) -> bool {
        self.expires.is_some_and(|expires| now >= expires)
    }
}

impl<K: Eq + Hash, V> ProviderCache<K, V> {
    /// An unbounded cache whose entries are kept for `duration`.
    pub(super) fn new(duration: Duration) -> Self {
        Self::with_limit(duration, None)
    }

    /// A cache whose entries are kept for `duration`, holding at most
    /// `limit` of the weight `weigh` gives each entry.
    pub(super) fn bounded(duration: Duration, limit: u64, weigh: fn(&V) -> u64) -> Self {
        Self::with_limit(duration, Some(SizeLimit { limit, weigh }))
    }

    fn with_limit(duration: Duration, limit: Option<SizeLimit<V>>) -> Self {
        Self {
            entries: HashMap::new(),
            duration,
            limit,
            size: 0,
            last_sweep: Instant::now(),
            uses: AtomicU64::new(0),
        }
    }

    /// The next use stamp.
    fn next_use(&self) -> u64 {
        self.uses.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// The response stored under `key`, unless it has expired
    /// (`TryGetValue`). Stamps it as the most recently used.
    pub(super) fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.get_at(key, Instant::now())
    }

    /// [`get`](Self::get) as of `now`, so that a check and the read after it
    /// agree on whether the entry has expired.
    pub(super) fn get_at<Q>(&self, key: &Q, now: Instant) -> Option<&V>
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        let entry = self.entries.get(key).filter(|entry| !entry.expired(now))?;
        entry.last_used.store(self.next_use(), Ordering::Relaxed);
        Some(&entry.value)
    }

    /// Whether a response is stored under `key` and has not expired.
    #[cfg(test)]
    pub(super) fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.contains_key_at(key, Instant::now())
    }

    /// Whether a response is stored under `key` and has not expired as of
    /// `now` (see [`get_at`](Self::get_at)). A read, so it stamps the entry
    /// as [`get`](Self::get) does.
    pub(super) fn contains_key_at<Q>(&self, key: &Q, now: Instant) -> bool
    where
        K: Borrow<Q>,
        Q: Eq + Hash + ?Sized,
    {
        self.get_at(key, now).is_some()
    }

    /// Stores `value` under `key`, in place of what it held (`Set`). A
    /// bounded cache with no room for it stores nothing, removes what `key`
    /// held and compacts itself; `value` comes back then, for the caller to
    /// use this once.
    pub(super) fn insert(&mut self, key: K, value: V) -> Option<V> {
        self.insert_at(key, value, Instant::now())
    }

    fn insert_at(&mut self, key: K, value: V, now: Instant) -> Option<V> {
        let weight = self.limit.as_ref().map_or(1, |limit| (limit.weigh)(&value));
        let prior = self.entries.get(&key).map_or(0, |entry| entry.weight);
        let over = self
            .limit
            .as_ref()
            .is_some_and(|limit| (self.size - prior).saturating_add(weight) > limit.limit);
        let rejected = if over {
            if let Some(entry) = self.entries.remove(&key) {
                self.size -= entry.weight;
            }
            self.compact(now);
            Some(value)
        } else {
            let entry = Entry {
                value,
                expires: now.checked_add(self.duration),
                weight,
                last_used: AtomicU64::new(self.next_use()),
            };
            if let Some(replaced) = self.entries.insert(key, entry) {
                self.size -= replaced.weight;
            }
            self.size += weight;
            None
        };
        self.sweep_if_due(now);
        rejected
    }

    /// Keeps only the entries `keep` accepts.
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&K, &V) -> bool) {
        self.retain_entries(|key, entry| keep(key, &entry.value));
    }

    fn retain_entries(&mut self, mut keep: impl FnMut(&K, &Entry<V>) -> bool) {
        let mut removed = 0;
        self.entries.retain(|key, entry| {
            let kept = keep(key, entry);
            if !kept {
                removed += entry.weight;
            }
            kept
        });
        self.size -= removed;
    }

    /// `StartScanForExpiredItemsIfNeeded`: the expired entries go once more
    /// than [`EXPIRATION_SCAN_FREQUENCY`] has passed since the last sweep.
    fn sweep_if_due(&mut self, now: Instant) {
        if now.saturating_duration_since(self.last_sweep) > EXPIRATION_SCAN_FREQUENCY {
            self.last_sweep = now;
            self.retain_entries(|_, entry| !entry.expired(now));
        }
    }

    /// `OvercapacityCompaction`: above the low watermark
    /// ([`COMPACTION_PERCENT`] below the limit), `Compact` removes the
    /// expired entries, then the least recently used ones, until it has
    /// removed the weight the cache holds above the watermark.
    fn compact(&mut self, now: Instant) {
        let Some(limit) = &self.limit else {
            return;
        };
        let low_watermark = limit.limit - limit.limit.saturating_mul(COMPACTION_PERCENT) / 100;
        if self.size <= low_watermark {
            return;
        }
        let target = self.size - low_watermark;
        let mut removed: u64 = self
            .entries
            .values()
            .filter(|entry| entry.expired(now))
            .map(|entry| entry.weight)
            .sum();
        // Stamps are unique, so the stamp of the last entry to go marks
        // exactly the entries `ExpirePriorityBucket` takes: every live one
        // used no later than it.
        let mut last_to_go = None;
        if removed < target {
            let mut live: Vec<(u64, u64)> = self
                .entries
                .values()
                .filter(|entry| !entry.expired(now))
                .map(|entry| (entry.last_used.load(Ordering::Relaxed), entry.weight))
                .collect();
            live.sort_unstable();
            for (stamp, weight) in live {
                last_to_go = Some(stamp);
                removed += weight;
                if removed >= target {
                    break;
                }
            }
        }
        self.retain_entries(|_, entry| {
            !entry.expired(now)
                && last_to_go.is_none_or(|stamp| entry.last_used.load(Ordering::Relaxed) > stamp)
        });
    }

    /// Whether the cache stores nothing, expired entries included.
    #[cfg(test)]
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The cache as it stands once `elapsed` has passed: every expiry and
    /// the last sweep that much earlier.
    #[cfg(test)]
    pub(super) fn pass(&mut self, elapsed: Duration) {
        for entry in self.entries.values_mut() {
            entry.expires = entry
                .expires
                .map(|at| at.checked_sub(elapsed).unwrap_or(at));
        }
        self.last_sweep = self
            .last_sweep
            .checked_sub(elapsed)
            .unwrap_or(self.last_sweep);
    }
}

#[cfg(test)]
mod tests {
    use super::{COMPACTION_PERCENT, EXPIRATION_SCAN_FREQUENCY, ProviderCache};
    use std::time::{Duration, Instant};

    const HOUR: Duration = Duration::from_hours(1);

    /// A cache bounded to `limit`, weighing each value as itself.
    fn bounded(limit: u64) -> ProviderCache<&'static str, u64> {
        ProviderCache::bounded(HOUR, limit, |weight| *weight)
    }

    #[test]
    fn an_entry_is_read_until_its_duration_has_passed_since_it_was_stored() {
        let start = Instant::now();
        let mut cache = ProviderCache::new(HOUR);
        assert!(cache.insert_at("series", 7, start).is_none());
        assert_eq!(cache.get_at(&"series", start + HOUR / 2), Some(&7));
        // Reading it does not extend it: the expiration is absolute.
        assert_eq!(
            cache.get_at(
                &"series",
                start + HOUR.saturating_sub(Duration::from_nanos(1))
            ),
            Some(&7)
        );
        assert_eq!(cache.get_at(&"series", start + HOUR), None, "expired");
        // A store replaces it with a fresh hour.
        cache.insert_at("series", 8, start + HOUR);
        assert_eq!(
            cache.get_at(&"series", start + HOUR + HOUR * 3 / 4),
            Some(&8)
        );
    }

    #[test]
    fn a_write_sweeps_the_expired_entries_out_once_a_minute() {
        let start = Instant::now();
        let mut cache = ProviderCache::new(Duration::from_secs(10));
        cache.last_sweep = start;
        cache.insert_at("old", 1, start);
        // Expired, but no sweep is due: it is still held, though never read.
        cache.insert_at("new", 2, start + EXPIRATION_SCAN_FREQUENCY);
        assert_eq!(cache.entries.len(), 2);
        assert_eq!(
            cache.get_at(&"old", start + EXPIRATION_SCAN_FREQUENCY),
            None
        );
        // Past the scan frequency, the next write sweeps it out.
        let later = start + EXPIRATION_SCAN_FREQUENCY + Duration::from_secs(1);
        cache.insert_at("newer", 3, later);
        assert!(!cache.entries.contains_key("old"));
        assert_eq!(cache.get_at(&"new", later), Some(&2), "not expired yet");
        assert_eq!(cache.get_at(&"newer", later), Some(&3));
        assert_eq!(cache.size, 2);
    }

    #[test]
    fn a_bounded_cache_holds_entries_up_to_its_limit() {
        let start = Instant::now();
        let mut cache = bounded(100);
        assert!(cache.insert_at("a", 60, start).is_none());
        assert!(
            cache.insert_at("b", 40, start).is_none(),
            "exactly the limit"
        );
        assert_eq!(cache.size, 100);
        // A replacement is weighed without the entry it replaces.
        assert!(cache.insert_at("b", 40, start).is_none());
        assert_eq!(cache.size, 100);
    }

    #[test]
    fn an_entry_with_no_room_is_not_stored_and_the_least_recently_used_go() {
        let start = Instant::now();
        let mut cache = bounded(100);
        cache.insert_at("a", 30, start);
        cache.insert_at("b", 30, start);
        cache.insert_at("c", 30, start);
        // `a` is read, so `b` is the least recently used now.
        assert_eq!(cache.get_at(&"a", start), Some(&30));
        assert_eq!(
            cache.insert_at("d", 20, start),
            Some(20),
            "no room: handed back"
        );
        // Compacted to the low watermark (95): 90 held, 0 to remove.
        assert_eq!(cache.size, 90, "below the watermark nothing goes");
        assert!(cache.get_at(&"d", start).is_none(), "not stored");

        let mut cache = bounded(100);
        cache.insert_at("a", 40, start);
        cache.insert_at("b", 30, start);
        cache.insert_at("c", 28, start);
        assert_eq!(cache.get_at(&"a", start), Some(&40));
        assert_eq!(cache.insert_at("d", 10, start), Some(10));
        // 98 held, 3 above the watermark: `b`, used longest ago, goes.
        assert_eq!(cache.size, 68);
        assert!(cache.get_at(&"b", start).is_none());
        assert_eq!(cache.get_at(&"a", start), Some(&40));
        assert_eq!(cache.get_at(&"c", start), Some(&28));
        // The next store of `d` has room.
        assert!(cache.insert_at("d", 10, start).is_none());
        assert_eq!(cache.size, 78);
    }

    #[test]
    fn compaction_takes_the_expired_entries_first() {
        let start = Instant::now();
        let mut cache = ProviderCache::bounded(Duration::from_secs(10), 100, |weight| *weight);
        cache.insert_at("old", 10, start);
        let later = start + Duration::from_secs(20);
        cache.insert_at("a", 50, later);
        cache.insert_at("b", 38, later);
        // 98 held, 3 above the watermark: the expired `old` covers it, and
        // no live entry goes.
        assert_eq!(cache.insert_at("c", 5, later), Some(5));
        assert!(!cache.entries.contains_key("old"));
        assert_eq!(cache.get_at(&"a", later), Some(&50));
        assert_eq!(cache.get_at(&"b", later), Some(&38));
        assert_eq!(cache.size, 88);
    }

    #[test]
    fn an_entry_replaced_without_room_is_removed() {
        let start = Instant::now();
        let mut cache = bounded(100);
        cache.insert_at("a", 50, start);
        cache.insert_at("b", 40, start);
        // `b` grown past the room left: neither the new nor the old value
        // stays (the prior entry is removed), and 50 held is below the
        // watermark.
        assert_eq!(cache.insert_at("b", 60, start), Some(60));
        assert!(cache.get_at(&"b", start).is_none());
        assert_eq!(cache.get_at(&"a", start), Some(&50));
        assert_eq!(cache.size, 50);
    }

    #[test]
    fn the_low_watermark_is_the_compaction_percentage_below_the_limit() {
        // `sizeLimit - (long)(sizeLimit * CompactionPercentage)` at 100 000.
        assert_eq!(100_000 - 100_000 * COMPACTION_PERCENT / 100, 95_000);
    }

    #[test]
    fn retain_drops_the_weight_of_what_it_removes() {
        let start = Instant::now();
        let mut cache = bounded(100);
        cache.insert_at("a", 30, start);
        cache.insert_at("b", 20, start);
        cache.retain(|key, _| *key != "a");
        assert_eq!(cache.size, 20);
        assert!(cache.get_at(&"a", start).is_none());
        assert!(!cache.is_empty());
    }
}
